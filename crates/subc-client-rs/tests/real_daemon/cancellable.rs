use super::*;
use std::{future::Future, pin::Pin};
use subc_client_rs::CancelOutcome;
use subc_protocol::{
    session::{ModuleControlRequest, ModuleControlResponse},
    ModuleHelloBody, PROTOCOL_VERSION,
};

const PROVIDER: &str = "cancellable-unary-provider";
const NO_FRAME_WINDOW: Duration = Duration::from_millis(150);

struct HeldCall {
    consumer: SubcConsumer,
    route: RouteHandle,
    identity: BindIdentity,
    provider: TcpStream,
    _daemon: LiveDaemon,
    _temp: ScratchDir,
}

impl HeldCall {
    async fn start() -> Self {
        let workspace = workspace_root();
        let daemon_bin = ensure_binary(
            &workspace,
            &[
                "build",
                "--locked",
                "-p",
                "subc-core",
                "--bin",
                "ck-subc",
                "--message-format=json",
            ],
        );
        let temp = unique_temp_dir("subc-client-rs-cancellable");
        let runtime = temp.join("runtime");
        let config = temp.join("config");
        fs::create_dir_all(&runtime).unwrap();
        write_empty_config(&config);
        // spawn_daemon isolates data, runtime and config under this scratch tree.
        let daemon = spawn_daemon(&daemon_bin, &runtime, &config);
        wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
        let mut provider = connect_authed_client(&daemon.connection_file)
            .await
            .unwrap();
        let hello = ModuleHelloBody {
            manifest: inline_module_manifest(PROVIDER, &["hold"]),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        };
        let hello = Frame::build(
            FrameType::Hello,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap();
        write_frame(&mut provider, &hello).await.unwrap();
        assert_eq!(
            read_frame_timeout(&mut provider).await.header.ty,
            FrameType::HelloAck
        );
        let consumer = SubcConsumer::connect(&daemon.connection_file, ConsumerOptions::default())
            .await
            .unwrap();
        let project = temp.join("project");
        fs::create_dir_all(&project).unwrap();
        let identity = BindIdentity::new(project, "cancellable", "test");
        let opening = consumer.open_route(target(), identity.clone(), CallOptions::default());
        let binding = async {
            let bind = read_frame_timeout(&mut provider).await;
            acknowledge_bind(&mut provider, &bind).await;
        };
        let (route, ()) = tokio::join!(opening, binding);
        Self {
            consumer,
            route: route.unwrap(),
            identity,
            provider,
            _daemon: daemon,
            _temp: temp,
        }
    }
}

fn target() -> RouteTarget {
    tool_target(PROVIDER)
}

async fn acknowledge_bind(provider: &mut TcpStream, bind: &Frame) {
    assert_eq!(bind.header.ty, FrameType::Request);
    assert_eq!(bind.header.channel, 0);
    let request: ModuleControlRequest = serde_json::from_slice(&bind.body).unwrap();
    assert!(matches!(request, ModuleControlRequest::RouteBind { .. }));
    let response = Frame::build(
        FrameType::Response,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        bind.header.corr,
        serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap(),
    )
    .unwrap();
    write_frame(provider, &response).await.unwrap();
}

// Drive the lazy reply until its Request reaches the raw provider. Managed calls
// may first open a route; acknowledge those binds without consuming route frames.
async fn held_request<F>(provider: &mut TcpStream, mut reply: Pin<&mut F>) -> Frame
where
    F: Future<Output = Result<Vec<u8>, CallError>>,
{
    loop {
        let frame = tokio::select! {
            frame = read_frame_timeout(provider) => frame,
            result = &mut reply => panic!("held request unexpectedly settled: {result:?}"),
        };
        if frame.header.channel == 0 {
            acknowledge_bind(provider, &frame).await;
        } else {
            assert_eq!(frame.header.ty, FrameType::Request);
            return frame;
        }
    }
}

async fn assert_cancel(provider: &mut TcpStream, request: &Frame) {
    let cancel = read_frame_timeout(provider).await;
    assert_eq!(cancel.header.ty, FrameType::Cancel);
    assert_eq!(
        cancel.header.channel, request.header.channel,
        "Cancel channel"
    );
    assert_eq!(cancel.header.epoch, request.header.epoch, "Cancel epoch");
    assert_eq!(cancel.header.corr, request.header.corr, "Cancel corr");
    assert!(cancel.body.is_empty(), "Cancel must have an empty body");
    assert_no_frame(provider).await;
}

async fn assert_no_frame(provider: &mut TcpStream) {
    let unexpected = timeout(NO_FRAME_WINDOW, read_frame(provider)).await;
    assert!(
        unexpected.is_err(),
        "unexpected provider frame: {unexpected:?}"
    );
}

async fn terminal(provider: &mut TcpStream, request: &Frame, ty: FrameType, body: &[u8]) {
    let response = Frame::build(
        ty,
        request.header.flags,
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        body.to_vec(),
    )
    .unwrap();
    write_frame(provider, &response).await.unwrap();
}

#[tokio::test]
async fn cancellable_cancel_matches_held_request_route_and_corr_once() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) =
        held.consumer
            .request_cancellable(&held.route, b"hold".to_vec(), CallOptions::default());
    tokio::pin!(reply);
    let request = held_request(&mut held.provider, reply.as_mut()).await;
    assert_eq!(
        (request.header.channel, request.header.epoch),
        (held.route.channel, held.route.epoch)
    );
    assert_ne!(request.header.corr, 0);
    assert_eq!(cancel.cancel().await, CancelOutcome::Sent);
    assert_cancel(&mut held.provider, &request).await;
    terminal(
        &mut held.provider,
        &request,
        FrameType::Response,
        b"finished",
    )
    .await;
    assert_eq!(reply.await.unwrap(), b"finished");
}

#[tokio::test]
async fn cancellable_response_after_cancel_preserves_body() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) =
        held.consumer
            .request_cancellable(&held.route, b"hold".to_vec(), CallOptions::default());
    tokio::pin!(reply);
    let request = held_request(&mut held.provider, reply.as_mut()).await;
    assert_eq!(cancel.cancel().await, CancelOutcome::Sent);
    assert_cancel(&mut held.provider, &request).await;
    let body = b"final response\0with binary bytes\xff";
    terminal(&mut held.provider, &request, FrameType::Response, body).await;
    assert_eq!(reply.await.unwrap(), body);
}

#[tokio::test]
async fn cancellable_managed_call_returns_provider_cancelled_error() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) = held.consumer.call_cancellable(
        target(),
        held.identity.clone(),
        b"hold".to_vec(),
        CallOptions::default(),
    );
    tokio::pin!(reply);
    let request = held_request(&mut held.provider, reply.as_mut()).await;
    assert_eq!(cancel.cancel().await, CancelOutcome::Sent);
    assert_cancel(&mut held.provider, &request).await;
    let error =
        ErrorBody::new("cancelled", "turn stopped").with_detail(json!({"source": "provider"}));
    terminal(
        &mut held.provider,
        &request,
        FrameType::Error,
        &serde_json::to_vec(&error).unwrap(),
    )
    .await;
    match reply.await {
        Err(CallError::Module(actual)) => {
            assert_eq!(actual.code, error.code);
            assert_eq!(actual.message, error.message);
            assert_eq!(actual.detail, error.detail);
        }
        other => panic!("expected provider cancellation error, got {other:?}"),
    }
}

#[tokio::test]
async fn cancellable_concurrent_double_cancel_sends_one_frame() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) =
        held.consumer
            .request_cancellable(&held.route, b"hold".to_vec(), CallOptions::default());
    tokio::pin!(reply);
    let request = held_request(&mut held.provider, reply.as_mut()).await;
    let (first, second) = tokio::join!(cancel.cancel(), cancel.cancel());
    assert!(matches!(
        (first, second),
        (CancelOutcome::Sent, CancelOutcome::AlreadyCancelled)
            | (CancelOutcome::AlreadyCancelled, CancelOutcome::Sent)
    ));
    assert_cancel(&mut held.provider, &request).await;
    terminal(&mut held.provider, &request, FrameType::Response, b"done").await;
    assert_eq!(reply.await.unwrap(), b"done");
}

#[tokio::test]
async fn cancellable_drop_handle_sends_no_cancel() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) =
        held.consumer
            .request_cancellable(&held.route, b"hold".to_vec(), CallOptions::default());
    tokio::pin!(reply);
    let request = held_request(&mut held.provider, reply.as_mut()).await;
    drop(cancel);
    assert_no_frame(&mut held.provider).await;
    terminal(
        &mut held.provider,
        &request,
        FrameType::Response,
        b"uncancelled",
    )
    .await;
    assert_eq!(reply.await.unwrap(), b"uncancelled");
}

#[tokio::test]
async fn cancellable_after_response_sends_nothing() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) =
        held.consumer
            .request_cancellable(&held.route, b"hold".to_vec(), CallOptions::default());
    tokio::pin!(reply);
    let request = held_request(&mut held.provider, reply.as_mut()).await;
    terminal(
        &mut held.provider,
        &request,
        FrameType::Response,
        b"already done",
    )
    .await;
    assert_eq!(reply.await.unwrap(), b"already done");
    assert_eq!(cancel.cancel().await, CancelOutcome::Settled);
    assert_no_frame(&mut held.provider).await;
}

#[tokio::test]
async fn cancellable_drop_reply_sends_no_cancel() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) =
        held.consumer
            .request_cancellable(&held.route, b"hold".to_vec(), CallOptions::default());
    let mut reply = Box::pin(reply);
    let _request = held_request(&mut held.provider, reply.as_mut()).await;
    drop(reply);
    assert_eq!(cancel.cancel().await, CancelOutcome::Settled);
    assert_no_frame(&mut held.provider).await;
}

#[tokio::test]
async fn cancellable_after_route_close_reports_route_closed() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) =
        held.consumer
            .request_cancellable(&held.route, b"hold".to_vec(), CallOptions::default());
    tokio::pin!(reply);
    let _request = held_request(&mut held.provider, reply.as_mut()).await;
    held.consumer
        .close_handle(&held.route, CloseRouteOptions::default())
        .await
        .unwrap();
    assert!(reply.await.is_err());
    assert_eq!(cancel.cancel().await, CancelOutcome::RouteClosed);
    let goodbye = read_frame_timeout(&mut held.provider).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_no_frame(&mut held.provider).await;
}

#[tokio::test]
async fn cancellable_after_connection_close_reports_connection_dropped() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) =
        held.consumer
            .request_cancellable(&held.route, b"hold".to_vec(), CallOptions::default());
    tokio::pin!(reply);
    let _request = held_request(&mut held.provider, reply.as_mut()).await;
    held.consumer.close().await;
    assert!(reply.await.is_err());
    assert_eq!(cancel.cancel().await, CancelOutcome::ConnectionDropped);
    // The daemon may send route teardown, but must never relay a Cancel.
    if let Ok(Ok(Some(frame))) = timeout(READ_TIMEOUT, read_frame(&mut held.provider)).await {
        assert_eq!(frame.header.ty, FrameType::Goodbye);
    }
}

#[tokio::test]
async fn cancellable_reply_keeps_original_deadline_after_cancel() {
    let mut held = HeldCall::start().await;
    let opts = CallOptions {
        timeout: Duration::from_millis(500),
        ..CallOptions::default()
    };
    let (cancel, reply) = held
        .consumer
        .request_cancellable(&held.route, b"hold".to_vec(), opts);
    tokio::pin!(reply);
    let request = held_request(&mut held.provider, reply.as_mut()).await;
    assert_eq!(cancel.cancel().await, CancelOutcome::Sent);
    assert_cancel(&mut held.provider, &request).await;
    let error = timeout(Duration::from_secs(2), reply)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.outcome_cause(), Some(OutcomeUnknownCause::Deadline));
    assert_eq!(cancel.cancel().await, CancelOutcome::AlreadyCancelled);
    assert_no_frame(&mut held.provider).await;
}

#[tokio::test]
async fn cancellable_before_dispatch_remembers_cancel_and_unpolled_drop_finishes() {
    let mut held = HeldCall::start().await;
    let (cancel, reply) = held.consumer.call_cancellable(
        target(),
        held.identity.clone(),
        b"hold".to_vec(),
        CallOptions::default(),
    );
    assert_eq!(cancel.cancel().await, CancelOutcome::Requested);
    assert_no_frame(&mut held.provider).await;
    tokio::pin!(reply);
    let request = held_request(&mut held.provider, reply.as_mut()).await;
    assert_cancel(&mut held.provider, &request).await;
    terminal(
        &mut held.provider,
        &request,
        FrameType::Response,
        b"deferred cancel",
    )
    .await;
    assert_eq!(reply.await.unwrap(), b"deferred cancel");
    let (cancel, reply) =
        held.consumer
            .request_cancellable(&held.route, vec![], CallOptions::default());
    drop(reply);
    assert_eq!(cancel.cancel().await, CancelOutcome::Settled);
    assert_no_frame(&mut held.provider).await;
}
