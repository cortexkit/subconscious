use super::*;
use std::task::{Context, Poll};
use tokio::io::{DuplexStream, ReadBuf};
use tokio::sync::Notify;

struct Echo;
#[async_trait]
impl ModuleHandler for Echo {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
}

fn ack() -> ModuleHelloAckBody {
    ModuleHelloAckBody {
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: Vec::new(),
        subc_capabilities: Vec::new(),
        storage: None,
        machine_id: None,
    }
}

fn start<R, W, H>(
    reader: R,
    writer: W,
    handler: H,
    options: ServeOptions,
) -> (
    ModuleHandle,
    tokio::task::JoinHandle<Result<(), SubcModuleError>>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    H: ModuleHandler,
{
    let (tx, rx) = mpsc::channel(EGRESS_BUFFER);
    let handle = ModuleHandle::new(
        &ack(),
        tx.clone(),
        123,
        CancellationToken::new(),
        tokio::runtime::Handle::current(),
    );
    handle.install_route(handle.route_handle(1, 1)).unwrap();
    let future = connection_serve_future_with_options(
        reader,
        tx,
        Arc::new(handler),
        handle.clone(),
        tokio::spawn(drain_writer(writer, rx)),
        options,
    );
    (handle, tokio::spawn(future))
}

fn connected<H: ModuleHandler>(
    handler: H,
    options: ServeOptions,
) -> (
    DuplexStream,
    tokio::task::JoinHandle<Result<(), SubcModuleError>>,
) {
    let (module, daemon) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(module);
    let (_, future) = start(reader, writer, handler, options);
    (daemon, future)
}

async fn request(daemon: &mut DuplexStream) {
    let frame = Frame::build(
        FrameType::Request,
        data_flags(),
        1,
        1,
        42,
        b"request".to_vec(),
    )
    .unwrap();
    write_frame(daemon, &frame).await.unwrap();
}

async fn receive(daemon: &mut DuplexStream) -> Frame {
    timeout(Duration::from_secs(3), read_frame(daemon))
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

async fn connector(host: &str, bind: &str) {
    let listener = tokio::net::TcpListener::bind((bind, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stream = connect_endpoint(host, port).await.unwrap();
    let (accepted, _) = listener.accept().await.unwrap();
    assert_eq!(stream.peer_addr().unwrap(), accepted.local_addr().unwrap());
}

#[tokio::test]
async fn connector_unbracketed_ipv6() {
    connector("::1", "::1").await;
}
#[tokio::test]
async fn connector_bracketed_ipv6() {
    connector("[::1]", "::1").await;
}
#[tokio::test]
async fn connector_ipv4() {
    connector("127.0.0.1", "127.0.0.1").await;
}
#[tokio::test]
async fn connector_localhost() {
    connector("localhost", "127.0.0.1").await;
}

struct Panics;
#[async_trait]
impl ModuleHandler for Panics {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        tokio::task::yield_now().await;
        panic!("private request data");
    }
}

#[tokio::test]
async fn handler_panic_sends_redacted_correlated_error_and_fails_serve() {
    let (mut daemon, serve) = connected(Panics, ServeOptions::default());
    request(&mut daemon).await;
    let frame = receive(&mut daemon).await;
    assert_eq!(
        (
            frame.header.ty,
            frame.header.channel,
            frame.header.epoch,
            frame.header.corr
        ),
        (FrameType::Error, 1, 1, 42)
    );
    let error: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(error.code, "internal");
    assert_eq!(error.message, "module handler panicked");
    assert!(!String::from_utf8_lossy(&frame.body).contains("private request data"));
    assert!(matches!(
        timeout(Duration::from_secs(3), serve)
            .await
            .unwrap()
            .unwrap(),
        Err(SubcModuleError::HandlerPanic)
    ));
    assert!(read_frame(&mut daemon).await.unwrap().is_none());
}

#[tokio::test]
async fn terminal_send_failure_is_reported_to_dispatcher() {
    let dispatcher = RequestDispatcher::new();
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let handle = ModuleHandle::new(
        &ack(),
        tx.clone(),
        124,
        CancellationToken::new(),
        tokio::runtime::Handle::current(),
    );
    handle.install_route(handle.route_handle(1, 1)).unwrap();
    let frame = Frame::build(FrameType::Request, data_flags(), 1, 1, 42, Vec::new()).unwrap();
    spawn_data_request(frame, tx, Arc::new(Echo), dispatcher.clone(), handle).unwrap();
    timeout(Duration::from_secs(1), dispatcher.failed.cancelled())
        .await
        .unwrap();
    assert!(matches!(
        dispatcher.take_failure(),
        Some(SubcModuleError::WriterClosed)
    ));
}

struct Reply(HandlerOutcome);
#[async_trait]
impl ModuleHandler for Reply {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        self.0.clone()
    }
}

async fn reply(outcome: HandlerOutcome) -> Frame {
    let stop = ModuleStopHandle::new();
    let (mut daemon, serve) = connected(
        Reply(outcome),
        ServeOptions::default().with_stop_handle(stop.clone()),
    );
    request(&mut daemon).await;
    let response = receive(&mut daemon).await;
    stop.stop();
    assert_eq!(receive(&mut daemon).await.header.ty, FrameType::Goodbye);
    serve.await.unwrap().unwrap();
    response
}

fn assert_oversized(frame: Frame, size: usize) {
    assert_eq!(
        (
            frame.header.ty,
            frame.header.channel,
            frame.header.epoch,
            frame.header.corr
        ),
        (FrameType::Error, 1, 1, 42)
    );
    assert!(!frame.header.flags.is_binary());
    let error: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(error.code, "frame_too_large");
    assert_eq!(
        error.detail,
        Some(serde_json::json!({"size": size, "limit": subc_protocol::MAX_DELIVERED_FRAME_LEN}))
    );
}

#[tokio::test]
async fn response_at_limit_is_sent_and_one_byte_over_gets_fallback() {
    let len = subc_protocol::MAX_DELIVERED_FRAME_LEN - subc_protocol::HEADER_LEN;
    let frame = reply(HandlerOutcome::Response(vec![b'x'; len])).await;
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.body, vec![b'x'; len]);
    assert_oversized(
        reply(HandlerOutcome::Response(vec![b'x'; len + 1])).await,
        subc_protocol::MAX_DELIVERED_FRAME_LEN + 1,
    );
}

#[tokio::test]
async fn error_at_limit_is_sent_and_one_byte_over_gets_fallback() {
    let overhead = serde_json::to_vec(&ErrorBody::new("test", ""))
        .unwrap()
        .len();
    let len = subc_protocol::MAX_DELIVERED_FRAME_LEN - subc_protocol::HEADER_LEN - overhead;
    let frame = reply(HandlerOutcome::Error {
        code: "test".into(),
        message: "x".repeat(len),
    })
    .await;
    assert_eq!(frame.header.ty, FrameType::Error);
    assert_eq!(
        frame.body.len() + subc_protocol::HEADER_LEN,
        subc_protocol::MAX_DELIVERED_FRAME_LEN
    );
    assert_eq!(
        serde_json::from_slice::<ErrorBody>(&frame.body)
            .unwrap()
            .message,
        "x".repeat(len)
    );
    assert_oversized(
        reply(HandlerOutcome::Error {
            code: "test".into(),
            message: "x".repeat(len + 1),
        })
        .await,
        subc_protocol::MAX_DELIVERED_FRAME_LEN + 1,
    );
}

#[tokio::test]
async fn detailed_error_over_limit_gets_fallback() {
    let outcome = HandlerOutcome::ErrorWithDetail {
        code: "test".into(),
        message: "error".into(),
        detail: serde_json::json!({"data": "x".repeat(subc_protocol::MAX_DELIVERED_FRAME_LEN)}),
    };
    let size = if let HandlerOutcome::ErrorWithDetail {
        code,
        message,
        detail,
    } = &outcome
    {
        subc_protocol::HEADER_LEN
            + serde_json::to_vec(&ErrorBody::new(code, message).with_detail(detail.clone()))
                .unwrap()
                .len()
    } else {
        unreachable!()
    };
    assert_oversized(reply(outcome).await, size);
}

struct Held {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl ModuleHandler for Held {
    async fn handle(&self, ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        self.entered.notify_one();
        tokio::select! {
            () = ctx.cancelled() => return HandlerOutcome::Response(b"cancelled".to_vec()),
            () = self.release.notified() => {},
        }
        HandlerOutcome::Response(b"finished".to_vec())
    }
}

#[tokio::test]
async fn external_stop_finishes_inflight_handler_then_sends_goodbye() {
    let stop = ModuleStopHandle::new();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (mut daemon, serve) = connected(
        Held {
            entered: entered.clone(),
            release: release.clone(),
        },
        ServeOptions::default().with_stop_handle(stop.clone()),
    );
    request(&mut daemon).await;
    entered.notified().await;
    stop.stop();
    tokio::task::yield_now().await;
    assert!(!serve.is_finished());
    release.notify_one();
    assert_eq!(receive(&mut daemon).await.body, b"finished");
    assert_eq!(receive(&mut daemon).await.header.ty, FrameType::Goodbye);
    serve.await.unwrap().unwrap();
    assert!(read_frame(&mut daemon).await.unwrap().is_none());
}

#[tokio::test]
async fn late_reply_after_route_goodbye_does_not_fail_the_connection() {
    let entered = Arc::new(Notify::new());
    let (mut daemon, serve) = connected(
        Held {
            entered: entered.clone(),
            release: Arc::new(Notify::new()),
        },
        ServeOptions::default(),
    );
    request(&mut daemon).await;
    entered.notified().await;
    let goodbye = Frame::build(FrameType::Goodbye, data_flags(), 1, 1, 0, Vec::new()).unwrap();
    write_frame(&mut daemon, &goodbye).await.unwrap();
    tokio::task::yield_now().await;
    let ping = Frame::build(FrameType::Ping, control_flags(), 0, 0, 99, Vec::new()).unwrap();
    write_frame(&mut daemon, &ping).await.unwrap();
    let pong = receive(&mut daemon).await;
    assert_eq!((pong.header.ty, pong.header.corr), (FrameType::Pong, 99));
    let goodbye = Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, Vec::new()).unwrap();
    write_frame(&mut daemon, &goodbye).await.unwrap();
    serve.await.unwrap().unwrap();
}

#[tokio::test]
async fn external_stop_during_handshake_sends_goodbye_and_tears_down() {
    let manifest = ModuleManifest::builder("test", "0.1.0").build();
    let stop = ModuleStopHandle::new();
    let (module, mut daemon) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(register_connection(
        module,
        manifest,
        Echo,
        ServeOptions::default().with_stop_handle(stop.clone()),
    ));
    assert_eq!(receive(&mut daemon).await.header.ty, FrameType::Hello);
    stop.stop();
    assert_eq!(receive(&mut daemon).await.header.ty, FrameType::Goodbye);
    assert!(task.await.unwrap().unwrap().is_none());
    assert!(read_frame(&mut daemon).await.unwrap().is_none());
}

#[tokio::test]
async fn already_stopped_does_not_read_connection_file() {
    let stop = ModuleStopHandle::new();
    stop.stop();
    let manifest = ModuleManifest::builder("test", "0.1.0").build();
    ServeOptions::default()
        .with_stop_handle(stop)
        .serve_with(Path::new("nonexistent-connection-file"), manifest, Echo)
        .await
        .unwrap();
}

struct NeverFinishes {
    entered: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

#[async_trait]
impl ModuleHandler for NeverFinishes {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        struct OnDrop(Arc<AtomicBool>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _guard = OnDrop(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn external_stop_aborts_over_budget_handler_and_tears_down() {
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let stop = ModuleStopHandle::new();
    let budget = Duration::from_millis(50);
    let (mut daemon, serve) = connected(
        NeverFinishes {
            entered: entered.clone(),
            dropped: dropped.clone(),
        },
        ServeOptions::default()
            .with_stop_handle(stop.clone())
            .with_writer_drain_timeout(budget),
    );
    request(&mut daemon).await;
    entered.notified().await;
    let started = Instant::now();
    stop.stop();
    assert_eq!(receive(&mut daemon).await.header.ty, FrameType::Goodbye);
    serve.await.unwrap().unwrap();
    assert_eq!(started.elapsed(), budget);
    assert!(dropped.load(Ordering::SeqCst));
    assert!(read_frame(&mut daemon).await.unwrap().is_none());
}

struct PanickingHealth;
#[async_trait]
impl ModuleHandler for PanickingHealth {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
    async fn health(&self) -> HealthReport {
        panic!("private health data");
    }
}

#[tokio::test]
async fn health_panic_sends_redacted_error_and_fails_serve() {
    let (mut daemon, serve) = connected(PanickingHealth, ServeOptions::default());
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        43,
        serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).unwrap(),
    )
    .unwrap();
    write_frame(&mut daemon, &frame).await.unwrap();
    let frame = receive(&mut daemon).await;
    assert_eq!(
        (frame.header.ty, frame.header.channel, frame.header.corr),
        (FrameType::Error, 0, 43)
    );
    let error: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(error.code, "internal");
    assert_eq!(error.message, "module handler panicked");
    assert!(matches!(
        serve.await.unwrap(),
        Err(SubcModuleError::HandlerPanic)
    ));
}

struct ReadError(io::ErrorKind);
impl AsyncRead for ReadError {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::from(self.0)))
    }
}

struct FlushError(io::ErrorKind);
impl AsyncWrite for FlushError {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::from(self.0)))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn idle_eof_is_ok() {
    let (handle, serve) = start(
        tokio::io::empty(),
        tokio::io::sink(),
        Echo,
        ServeOptions::default(),
    );
    serve.await.unwrap().unwrap();
    assert!(handle.is_closed());
}

#[tokio::test]
async fn read_reset_and_abort_are_errors() {
    for kind in [
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::ConnectionAborted,
    ] {
        let (_, serve) = start(
            ReadError(kind),
            tokio::io::sink(),
            Echo,
            ServeOptions::default(),
        );
        assert!(
            matches!(serve.await.unwrap(), Err(SubcModuleError::FrameIo(FrameIoError::Io(error))) if error.kind() == kind)
        );
    }
}

#[tokio::test]
async fn write_disconnect_while_read_side_open_is_error() {
    for kind in [
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::ConnectionAborted,
    ] {
        let (module, mut daemon) = tokio::io::duplex(1024);
        let (_, serve) = start(module, FlushError(kind), Echo, ServeOptions::default());
        request(&mut daemon).await;
        assert!(
            matches!(timeout(Duration::from_secs(3), serve).await.unwrap().unwrap(), Err(SubcModuleError::FrameIo(FrameIoError::Io(error))) if error.kind() == kind)
        );
    }
}

#[tokio::test]
async fn orderly_eof_and_goodbye_allow_late_flush_disconnects() {
    for kind in [
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::ConnectionAborted,
    ] {
        let (_, serve) = start(
            tokio::io::empty(),
            FlushError(kind),
            Echo,
            ServeOptions::default(),
        );
        serve.await.unwrap().unwrap();
        let goodbye =
            Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, Vec::new()).unwrap();
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &goodbye).await.unwrap();
        let (_, serve) = start(
            std::io::Cursor::new(bytes),
            FlushError(kind),
            Echo,
            ServeOptions::default(),
        );
        serve.await.unwrap().unwrap();
    }
}

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                use std::fmt::Write;
                write!(&mut self.0, "{}={value:?} ", field.name()).unwrap();
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

#[tokio::test]
async fn eof_with_inflight_reply_and_broken_pipe_is_ok_and_logs_count_only() {
    let capture = LogCapture::default();
    let dispatch = tracing::Dispatch::new(capture.clone());
    let _guard = tracing::dispatcher::set_default(&dispatch);
    let entered = Arc::new(Notify::new());
    let (module, mut daemon) = tokio::io::duplex(1024);
    let (_, serve) = start(
        module,
        FlushError(io::ErrorKind::BrokenPipe),
        Held {
            entered: entered.clone(),
            release: Arc::new(Notify::new()),
        },
        ServeOptions::default(),
    );
    request(&mut daemon).await;
    entered.notified().await;
    daemon.shutdown().await.unwrap();
    serve.await.unwrap().unwrap();
    let logs = capture.0.lock().unwrap();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(
        logs[0].contains("daemon EOF: replies could not be delivered during drain"),
        "{logs:?}"
    );
    assert!(logs[0].contains("undelivered_replies=1"), "{logs:?}");
    assert!(!logs[0].contains("cancelled"));
}

struct StuckWriter(Arc<AtomicBool>);
impl Drop for StuckWriter {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl AsyncWrite for StuckWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Pending
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

#[tokio::test(start_paused = true)]
async fn configured_writer_drain_budget_is_honored_and_writer_is_aborted() {
    let dropped = Arc::new(AtomicBool::new(false));
    let budget = Duration::from_millis(37);
    let started = Instant::now();
    let (_, serve) = start(
        tokio::io::empty(),
        StuckWriter(dropped.clone()),
        Echo,
        ServeOptions::default().with_writer_drain_timeout(budget),
    );
    serve.await.unwrap().unwrap();
    assert_eq!(started.elapsed(), budget);
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(
        ServeOptions::default().writer_drain_timeout,
        Duration::from_secs(2)
    );
}
