fn selector() -> ScopeWatchSelector {
    ScopeWatchSelector { scope_ref: ScopeRef { owner: owner(), scope_ref: "s".into() }, epoch: 7 }
}

fn snapshot_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"op":"scope.watch","scopes":[{
        "scope_ref":{"owner":{"kind":"reserved","module_id":"owner"},"ref":"s"},
        "epoch":7,"status":"live","future":true
    }]})).unwrap()
}

async fn open_watch(handle: &ModuleHandle, rx: &mut mpsc::Receiver<Frame>) -> (ScopeSubscription, u64) {
    let caller = handle.clone();
    let pending = tokio::spawn(async move { caller.scope_watch(vec![selector()]).await });
    let request = timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap();
    assert_eq!(request.header.channel, 0);
    assert_eq!(request.header.epoch, 0);
    assert!(request.header.flags.is_subscription());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(), serde_json::json!({"op":"scope.watch","scopes":[{
        "scope_ref":{"owner":{"kind":"reserved","module_id":"owner"},"ref":"s"},"epoch":7
    }]}));
    assert!(handle.handle_control_reply(control_reply(FrameType::Response, request.header.corr, snapshot_body())));
    (pending.await.unwrap().unwrap(), request.header.corr)
}

#[tokio::test]
async fn scope_watch_module_snapshot_push_cancel_and_drop() {
    let (handle, mut rx) = test_module_handle(&[subc_control::ops::SCOPE_WATCH]);
    let (mut watch, corr) = open_watch(&handle, &mut rx).await;
    assert_eq!(watch.initial.scopes[0].status, ScopeWatchStatus::Live);
    let ended = serde_json::to_vec(&serde_json::json!({"op":"scope.watch","event":"ended","scope_ref":{"owner":{"kind":"reserved","module_id":"owner"},"ref":"s"},"epoch":7})).unwrap();
    assert!(handle.handle_control_reply(control_reply(FrameType::StreamData, corr, ended)));
    let event = watch.next().await.unwrap().unwrap();
    assert_eq!(event.event, ScopeWatchEvent::Ended);
    assert_eq!(event.epoch, 7);
    assert_eq!(event.by_epoch, None);
    watch.unsubscribe();
    assert!(watch.next().await.unwrap().is_none());
    let cancel = timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap();
    assert_eq!((cancel.header.ty, cancel.header.channel, cancel.header.corr), (FrameType::Cancel, 0, corr));
    let (watch, corr) = open_watch(&handle, &mut rx).await;
    drop(watch);
    assert_eq!(timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap().header.corr, corr);
    assert!(handle.shared.lock_inner().pending_control_streams.is_empty());
}

#[tokio::test]
async fn scope_watch_module_queued_events_precede_terminal_refusal() {
    let (handle, mut rx) = test_module_handle(&[subc_control::ops::SCOPE_WATCH]);
    let (mut watch, corr) = open_watch(&handle, &mut rx).await;
    let event = ScopeWatchPush::new(ScopeWatchEvent::Superseded, selector().scope_ref, 7, Some(9));
    handle.handle_control_reply(control_reply(FrameType::StreamData, corr, serde_json::to_vec(&event).unwrap()));
    handle.handle_control_reply(control_reply(FrameType::Error, corr, serde_json::to_vec(&serde_json::json!({"code":"future_refusal","message":"stopped"})).unwrap()));
    assert_eq!(watch.next().await.unwrap(), Some(event));
    assert_eq!(watch.next().await, Err(ScopeCallError::Refused { code: "future_refusal".into(), message: "stopped".into() }));
    assert!(watch.next().await.unwrap().is_none());
}

#[tokio::test]
async fn scope_watch_module_gates_op_and_preserves_open_refusal() {
    let (unsupported, mut rx) = test_module_handle(&[]);
    assert!(matches!(unsupported.scope_watch(vec![]).await, Err(ScopeCallError::NotSupported { op: "scope.watch" })));
    assert!(rx.try_recv().is_err());
    let (handle, mut rx) = test_module_handle(&[subc_control::ops::SCOPE_WATCH]);
    let caller = handle.clone();
    let pending = tokio::spawn(async move { caller.scope_watch(vec![]).await });
    let request = rx.recv().await.unwrap();
    handle.handle_control_reply(control_reply(FrameType::Error, request.header.corr, serde_json::to_vec(&serde_json::json!({"code":"scope_watch_limit_exceeded","message":"full"})).unwrap()));
    assert!(matches!(pending.await.unwrap(), Err(ScopeCallError::Refused { code, .. }) if code == "scope_watch_limit_exceeded"));
}

#[tokio::test]
async fn scope_watch_module_disconnect_and_invalid_event_end_feed() {
    let (handle, mut rx) = test_module_handle(&[subc_control::ops::SCOPE_WATCH]);
    let (mut watch, _) = open_watch(&handle, &mut rx).await;
    handle.close_connection();
    assert_eq!(watch.next().await, Err(ScopeCallError::ConnectionClosed));
    assert!(watch.next().await.unwrap().is_none());
    let (handle, mut rx) = test_module_handle(&[subc_control::ops::SCOPE_WATCH]);
    let (mut watch, corr) = open_watch(&handle, &mut rx).await;
    handle.handle_control_reply(control_reply(FrameType::StreamData, corr, snapshot_body()));
    assert!(matches!(watch.next().await, Err(ScopeCallError::Protocol(_))));
    assert!(watch.next().await.unwrap().is_none());
    assert_eq!(rx.recv().await.unwrap().header.ty, FrameType::Cancel);
}
