use super::*;
use std::{future::poll_fn, task::Poll};

struct Echo;

#[async_trait]
impl ModuleHandler for Echo {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
}

fn handle_on(
    runtime: tokio::runtime::Handle,
    ops: &[&str],
) -> (ModuleHandle, mpsc::Receiver<Frame>) {
    let (writer, rx) = mpsc::channel(1);
    let ack = ModuleHelloAckBody {
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: ops.iter().map(|op| (*op).to_owned()).collect(),
        subc_capabilities: Vec::new(),
        storage: None,
        machine_id: None,
    };
    (
        ModuleHandle::new(&ack, writer, 1, CancellationToken::new(), runtime),
        rx,
    )
}

fn handle() -> (ModuleHandle, mpsc::Receiver<Frame>) {
    handle_on(tokio::runtime::Handle::current(), &[OPERATOR_CONFIRM_OP])
}

fn reply(ty: FrameType, corr: u64, body: serde_json::Value) -> Frame {
    Frame::build(
        ty,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

fn confirmed(corr: u64) -> Frame {
    reply(
        FrameType::Response,
        corr,
        serde_json::json!({"op": "operator.confirm", "outcome": "confirmed"}),
    )
}

fn pending_count(handle: &ModuleHandle) -> usize {
    handle.shared.lock_inner().pending_operator_confirms.len()
}

async fn still_pending<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "confirmation completed early"
        );
        Poll::Ready(())
    })
    .await;
}

fn fill_egress(handle: &ModuleHandle) {
    handle
        .shared
        .lock_inner()
        .writer
        .as_ref()
        .unwrap()
        .try_send(confirmed(999))
        .unwrap();
}

async fn assert_one_cancel(rx: &mut mpsc::Receiver<Frame>, corr: u64) {
    let cancel = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancel.header.ty, FrameType::Cancel);
    assert_eq!(cancel.header.channel, 0);
    assert_eq!(cancel.header.epoch, 0);
    assert_eq!(cancel.header.corr, corr);
    assert!(cancel.body.is_empty());
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err(), "more than one Cancel was sent");
}

#[tokio::test]
async fn operator_confirm_fake_daemon_confirmed_reply_succeeds() {
    let mut served =
        super::module_close_tests::serve_against_stand_in_with_ops(Echo, &[OPERATOR_CONFIRM_OP])
            .await;
    let route = served.handle.route_handle(7, 3);
    let mut call = Box::pin(served.handle.confirm_operator("Replace identity", &route));
    still_pending(call.as_mut()).await;
    let request = timeout(Duration::from_secs(2), read_frame(&mut served.daemon))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(request.header.ty, FrameType::Request);
    assert_eq!(request.header.channel, 0);
    assert_eq!(request.header.epoch, 0);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
        serde_json::json!({"op": "operator.confirm", "summary": "Replace identity",
            "route_channel": 7, "route_epoch": 3})
    );
    write_frame(&mut served.daemon, &confirmed(request.header.corr))
        .await
        .unwrap();
    served.daemon.flush().await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), &mut call).await.unwrap(),
        Ok(())
    );
    assert_eq!(pending_count(&served.handle), 0);
    drop(call);
    served.handle.close_connection();
    timeout(Duration::from_secs(3), served.serve)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn operator_confirm_fake_daemon_close_wakes_before_deadline() {
    let mut served =
        super::module_close_tests::serve_against_stand_in_with_ops(Echo, &[OPERATOR_CONFIRM_OP])
            .await;
    let route = served.handle.route_handle(7, 3);
    let mut call = Box::pin(served.handle.confirm_operator("Replace identity", &route));
    still_pending(call.as_mut()).await;
    let _ = timeout(Duration::from_secs(2), read_frame(&mut served.daemon))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    served.daemon.shutdown().await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), &mut call).await.unwrap(),
        Err(OperatorConfirmError::PresenceUnavailable)
    );
    assert_eq!(pending_count(&served.handle), 0);
    drop(call);
    timeout(Duration::from_secs(3), served.serve)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn operator_confirm_unadvertised_is_unsupported_without_sending() {
    let (handle, mut rx) = handle_on(tokio::runtime::Handle::current(), &[]);
    assert_eq!(
        handle
            .confirm_operator("Replace identity", &handle.route_handle(7, 3))
            .await,
        Err(OperatorConfirmError::Unsupported)
    );
    assert_eq!(pending_count(&handle), 0);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn operator_confirm_foreign_route_is_not_permitted_without_sending() {
    let (handle, mut rx) = handle();
    assert_eq!(
        handle
            .confirm_operator("Replace identity", &RouteHandle::new(7, 3, 2))
            .await,
        Err(OperatorConfirmError::NotPermitted)
    );
    assert_eq!(pending_count(&handle), 0);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn operator_confirm_daemon_errors_map_to_unit_variants() {
    let cases = [
        ("operator_declined", OperatorConfirmError::Declined),
        (
            "operator_presence_unavailable",
            OperatorConfirmError::PresenceUnavailable,
        ),
        (
            "operator_summary_invalid",
            OperatorConfirmError::SummaryInvalid,
        ),
        (
            "operator_request_not_permitted",
            OperatorConfirmError::NotPermitted,
        ),
        ("not_registered", OperatorConfirmError::NotPermitted),
        (
            "unsupported_control_frame",
            OperatorConfirmError::Unsupported,
        ),
        ("unknown_code", OperatorConfirmError::PresenceUnavailable),
    ];
    for (code, expected) in cases {
        let (handle, mut rx) = handle();
        let route = handle.route_handle(7, 3);
        let mut call = Box::pin(handle.confirm_operator("Replace identity", &route));
        still_pending(call.as_mut()).await;
        let request = rx.recv().await.unwrap();
        assert!(handle.handle_control_reply(reply(FrameType::Error, request.header.corr,
            serde_json::json!({"code": code, "message": "refused", "detail": {"reason": "audit only"}}))));
        assert_eq!(call.await, Err(expected), "{code}");
        assert_eq!(pending_count(&handle), 0);
        assert!(
            rx.try_recv().is_err(),
            "terminal reply must disarm cancellation"
        );
    }
}

#[tokio::test]
async fn operator_confirm_only_well_formed_confirmed_reply_succeeds() {
    let cases = [
        (
            FrameType::Response,
            serde_json::json!({"op": "operator.confirm", "outcome": "declined"}),
        ),
        (
            FrameType::Response,
            serde_json::json!({"op": "catalog.update", "outcome": "confirmed"}),
        ),
        (
            FrameType::Response,
            serde_json::json!({"outcome": "confirmed"}),
        ),
        (
            FrameType::Response,
            serde_json::json!({"op": "operator.confirm"}),
        ),
        (FrameType::Response, serde_json::json!(["not a reply"])),
        (
            FrameType::Error,
            serde_json::json!({"code": "operator_declined"}),
        ),
        (
            FrameType::Push,
            serde_json::json!({"op": "operator.confirm", "outcome": "confirmed"}),
        ),
    ];
    for (ty, body) in cases {
        let (handle, mut rx) = handle();
        let route = handle.route_handle(7, 3);
        let mut call = Box::pin(handle.confirm_operator("Replace identity", &route));
        still_pending(call.as_mut()).await;
        let request = rx.recv().await.unwrap();
        assert!(handle.handle_control_reply(reply(ty, request.header.corr, body)));
        assert_eq!(call.await, Err(OperatorConfirmError::PresenceUnavailable));
        assert!(rx.try_recv().is_err());
    }
}

#[tokio::test]
async fn operator_confirm_uses_separate_table_and_shared_corr_allocator() {
    let (handle, mut rx) = handle();
    let (catalog_corr, _, catalog_rx) = handle.shared.begin_catalog_update().unwrap();
    let route = handle.route_handle(7, 3);
    let mut call = Box::pin(handle.confirm_operator("Replace identity", &route));
    still_pending(call.as_mut()).await;
    let request = rx.recv().await.unwrap();
    assert!(request.header.corr > catalog_corr);
    assert_eq!(pending_count(&handle), 1);
    assert_eq!(handle.shared.lock_inner().pending_catalog_updates.len(), 1);
    handle.handle_control_reply(confirmed(request.header.corr));
    assert_eq!(call.await, Ok(()));
    handle.handle_control_reply(reply(
        FrameType::Response,
        catalog_corr,
        serde_json::json!({"op": "catalog.update"}),
    ));
    assert!(matches!(
        catalog_rx.await.unwrap(),
        Ok(ModuleControlResponseToModule::CatalogUpdate {})
    ));
}

#[tokio::test(start_paused = true)]
async fn operator_confirm_still_waits_after_eleven_seconds() {
    let (handle, mut rx) = handle();
    let route = handle.route_handle(7, 3);
    let mut call = Box::pin(handle.confirm_operator("Replace identity", &route));
    still_pending(call.as_mut()).await;
    let request = rx.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(11)).await;
    still_pending(call.as_mut()).await;
    handle.handle_control_reply(confirmed(request.header.corr));
    assert_eq!(call.await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn operator_confirm_deadline_includes_wait_for_egress_capacity() {
    let (handle, mut rx) = handle();
    fill_egress(&handle);
    let route = handle.route_handle(7, 3);
    let mut call = Box::pin(handle.confirm_operator_with_deadline(
        "Replace identity",
        &route,
        Duration::from_secs(5),
    ));
    still_pending(call.as_mut()).await;
    assert_eq!(pending_count(&handle), 1);
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(
        timeout(Duration::from_secs(1), &mut call)
            .await
            .expect("deadline did not cover send"),
        Err(OperatorConfirmError::PresenceUnavailable)
    );
    assert_eq!(pending_count(&handle), 0);
    assert_eq!(rx.recv().await.unwrap().header.corr, 999);
    assert!(
        rx.try_recv().is_err(),
        "unaccepted request must not send Cancel"
    );
}

#[tokio::test(start_paused = true)]
async fn operator_confirm_capacity_wait_reduces_reply_budget() {
    let (handle, mut rx) = handle();
    fill_egress(&handle);
    let route = handle.route_handle(7, 3);
    let mut call = Box::pin(handle.confirm_operator_with_deadline(
        "Replace identity",
        &route,
        Duration::from_secs(100),
    ));
    still_pending(call.as_mut()).await;
    tokio::time::advance(Duration::from_secs(70)).await;
    assert_eq!(rx.recv().await.unwrap().header.corr, 999);
    still_pending(call.as_mut()).await;
    let request = rx.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(29)).await;
    still_pending(call.as_mut()).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        timeout(Duration::from_secs(1), &mut call)
            .await
            .expect("send reset the reply budget"),
        Err(OperatorConfirmError::PresenceUnavailable)
    );
    assert_eq!(pending_count(&handle), 0);
    assert_one_cancel(&mut rx, request.header.corr).await;
}

#[tokio::test]
async fn operator_confirm_drop_before_acceptance_cleans_table_without_cancel() {
    let (handle, mut rx) = handle();
    fill_egress(&handle);
    let route = handle.route_handle(7, 3);
    let mut call = Box::pin(handle.confirm_operator("Replace identity", &route));
    still_pending(call.as_mut()).await;
    assert_eq!(pending_count(&handle), 1);
    drop(call);
    assert_eq!(pending_count(&handle), 0);
    assert_eq!(rx.recv().await.unwrap().header.corr, 999);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn operator_confirm_drop_after_acceptance_sends_one_cancel() {
    let (handle, mut rx) = handle();
    let route = handle.route_handle(7, 3);
    let mut call = Box::pin(handle.confirm_operator("Replace identity", &route));
    still_pending(call.as_mut()).await;
    let request = rx.recv().await.unwrap();
    drop(call);
    assert_eq!(pending_count(&handle), 0);
    assert_one_cancel(&mut rx, request.header.corr).await;
}

#[tokio::test]
async fn operator_confirm_drop_with_full_egress_sends_one_cancel_when_drained() {
    let (handle, mut rx) = handle();
    let route = handle.route_handle(7, 3);
    let mut call = Box::pin(handle.confirm_operator("Replace identity", &route));
    still_pending(call.as_mut()).await;
    let request = rx.recv().await.unwrap();
    fill_egress(&handle);
    drop(call);
    assert_eq!(pending_count(&handle), 0);
    assert_eq!(rx.recv().await.unwrap().header.corr, 999);
    assert_one_cancel(&mut rx, request.header.corr).await;
}

#[tokio::test]
async fn operator_confirm_plain_thread_drop_uses_connection_runtime() {
    let (handle, mut rx) = handle();
    let owned_handle = handle.clone();
    let mut call = Box::pin(async move {
        owned_handle
            .confirm_operator("Replace identity", &owned_handle.route_handle(7, 3))
            .await
    });
    still_pending(call.as_mut()).await;
    let request = rx.recv().await.unwrap();
    fill_egress(&handle);
    std::thread::spawn(move || drop(call))
        .join()
        .expect("drop panicked on a plain thread");
    assert_eq!(pending_count(&handle), 0);
    assert_eq!(rx.recv().await.unwrap().header.corr, 999);
    assert_one_cancel(&mut rx, request.header.corr).await;
}

#[test]
fn operator_confirm_second_runtime_shutdown_uses_connection_runtime() {
    let connection_runtime = tokio::runtime::Runtime::new().unwrap();
    let (handle, mut rx) = handle_on(connection_runtime.handle().clone(), &[OPERATOR_CONFIRM_OP]);
    let polling_runtime = tokio::runtime::Runtime::new().unwrap();
    let owned_handle = handle.clone();
    polling_runtime.spawn(async move {
        owned_handle
            .confirm_operator("Replace identity", &owned_handle.route_handle(7, 3))
            .await
    });
    let request = connection_runtime.block_on(async {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap()
    });
    fill_egress(&handle);
    drop(polling_runtime);
    assert_eq!(pending_count(&handle), 0);
    connection_runtime.block_on(async {
        assert_eq!(rx.recv().await.unwrap().header.corr, 999);
        assert_one_cancel(&mut rx, request.header.corr).await;
    });
}

#[tokio::test]
async fn operator_confirm_late_reply_cannot_resolve_another_call() {
    let (handle, mut rx) = handle();
    let route = handle.route_handle(7, 3);
    let mut first = Box::pin(handle.confirm_operator("First write", &route));
    still_pending(first.as_mut()).await;
    let first_corr = rx.recv().await.unwrap().header.corr;
    drop(first);
    assert_one_cancel(&mut rx, first_corr).await;
    let mut second = Box::pin(handle.confirm_operator("Second write", &route));
    still_pending(second.as_mut()).await;
    let second_corr = rx.recv().await.unwrap().header.corr;
    assert_ne!(second_corr, first_corr);
    assert!(!handle.handle_control_reply(confirmed(first_corr)));
    still_pending(second.as_mut()).await;
    handle.handle_control_reply(confirmed(second_corr));
    assert_eq!(second.await, Ok(()));
}

#[tokio::test]
async fn operator_confirm_close_drains_pending_table_and_wakes_waiter() {
    let (handle, mut rx) = handle();
    let route = handle.route_handle(7, 3);
    let mut call = Box::pin(handle.confirm_operator("Replace identity", &route));
    still_pending(call.as_mut()).await;
    let _ = rx.recv().await.unwrap();
    handle.close_connection();
    assert_eq!(pending_count(&handle), 0);
    assert_eq!(
        timeout(Duration::from_secs(2), &mut call).await.unwrap(),
        Err(OperatorConfirmError::PresenceUnavailable)
    );
}

#[tokio::test]
async fn operator_confirm_corr_exhaustion_drains_both_tables_from_either_allocator() {
    for catalog_exhausts in [true, false] {
        let (handle, mut rx) = handle();
        let (_, _, catalog_rx) = handle.shared.begin_catalog_update().unwrap();
        let route = handle.route_handle(7, 3);
        let mut call = Box::pin(handle.confirm_operator("Replace identity", &route));
        still_pending(call.as_mut()).await;
        let _ = rx.recv().await.unwrap();
        handle.shared.lock_inner().next_corr = None;
        if catalog_exhausts {
            assert!(matches!(
                handle.shared.begin_catalog_update(),
                Err(CatalogUpdateError::ConnectionClosed)
            ));
        } else {
            assert!(matches!(
                handle.shared.begin_operator_confirm(),
                Err(OperatorConfirmError::PresenceUnavailable)
            ));
        }
        assert!(handle.is_closed());
        assert_eq!(pending_count(&handle), 0);
        assert!(handle
            .shared
            .lock_inner()
            .pending_catalog_updates
            .is_empty());
        assert_eq!(
            timeout(Duration::from_secs(2), &mut call).await.unwrap(),
            Err(OperatorConfirmError::PresenceUnavailable)
        );
        assert!(matches!(
            catalog_rx.await.unwrap(),
            Err(CatalogUpdateError::ConnectionClosed)
        ));
    }
}

#[tokio::test]
async fn operator_confirm_closed_writer_fails_without_leaking_pending_entry() {
    let (handle, rx) = handle();
    drop(rx);
    assert_eq!(
        handle
            .confirm_operator("Replace identity", &handle.route_handle(7, 3))
            .await,
        Err(OperatorConfirmError::PresenceUnavailable)
    );
    assert_eq!(pending_count(&handle), 0);
}
