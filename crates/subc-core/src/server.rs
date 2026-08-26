use std::{error::Error, fmt, io, net::SocketAddr, sync::Arc, time::Duration};

use subc_transport::{authenticate_server, AuthError, DAEMON_ID_LEN, WATCHDOG_CLIENT_ROLE};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    net::TcpListener,
    sync::{mpsc, Semaphore},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, warn};

use crate::{
    forwarding::{CloseReason, ConnectionCloseReceiver},
    observability::ConnectedClients,
    read_frame,
    router::{FrameSink, RouteCtx, Router},
    write_frame, FrameIoError, RouterError,
};

pub const CONNECTION_EGRESS_BUFFER: usize = 64;
pub const DEFAULT_AUTH_DEADLINE: Duration = Duration::from_secs(2);
// Sized for the restart-herd shape: after a daemon bounce, every live client
// connection plus all supervised children re-dial within the same second
// (~120+ observed on the 2026-07-14 fleet). Handshakes are cheap loopback
// HMAC exchanges; the deadline, not the permit count, is the DoS bound.
pub const DEFAULT_MAX_UNAUTHENTICATED_CONNECTIONS: usize = 256;
const CLOSE_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Authentication material and DoS bounds applied before a TCP connection may
/// reach the frame router.
#[derive(Clone)]
pub struct ServerAuth {
    key: Arc<[u8]>,
    daemon_id: [u8; DAEMON_ID_LEN],
    daemon_ver: Arc<str>,
    deadline: Duration,
    unauthenticated: Arc<Semaphore>,
    connected_clients: ConnectedClients,
}

impl ServerAuth {
    pub fn new(
        key: Vec<u8>,
        daemon_id: [u8; DAEMON_ID_LEN],
        daemon_ver: impl Into<String>,
    ) -> Self {
        Self::with_limits(
            key,
            daemon_id,
            daemon_ver,
            DEFAULT_AUTH_DEADLINE,
            DEFAULT_MAX_UNAUTHENTICATED_CONNECTIONS,
        )
    }

    // Production limits are deliberately not config-routed: loosening pre-auth DoS posture changes attack surface.
    pub fn with_limits(
        key: Vec<u8>,
        daemon_id: [u8; DAEMON_ID_LEN],
        daemon_ver: impl Into<String>,
        deadline: Duration,
        max_unauthenticated: usize,
    ) -> Self {
        Self {
            key: Arc::from(key),
            daemon_id,
            daemon_ver: Arc::from(daemon_ver.into()),
            deadline,
            unauthenticated: Arc::new(Semaphore::new(max_unauthenticated.max(1))),
            connected_clients: ConnectedClients::new(),
        }
    }

    pub fn with_connected_clients(mut self, connected_clients: ConnectedClients) -> Self {
        self.connected_clients = connected_clients;
        self
    }
}

impl fmt::Debug for ServerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerAuth")
            .field("key", &"<redacted>")
            .field("daemon_id", &self.daemon_id)
            .field("daemon_ver", &self.daemon_ver)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// Serve an already-bound TCP listener. Each accepted connection gets its own
/// async task so concurrent clients do not block the accept loop.
pub async fn serve_listener(
    listener: TcpListener,
    router: Arc<Router>,
    auth: ServerAuth,
) -> Result<(), ServerError> {
    let local_addr = listener.local_addr().ok();
    loop {
        let (stream, peer_addr) = listener
            .accept()
            .await
            .map_err(|source| ServerError::Accept { local_addr, source })?;
        // Every route frame is a discrete message whose reply the peer is waiting
        // for, so there is never a later write for Nagle to coalesce with -- it can
        // only hold a frame back until an ACK arrives.
        //
        // MEASURED: no effect on this transport. A 50-sample-per-arm sweep from
        // 1 to 32 KiB over the full client->daemon->module->client path showed a
        // flat 0.28-0.38ms p50 with no step at any buffer boundary, because a
        // loopback ACK returns in microseconds and never reaches the delayed-ACK
        // timer that makes Nagle expensive on a real network. Kept anyway: it is
        // one syscall at accept, it removes the mechanism rather than relying on
        // loopback staying fast, and Windows loopback was not part of that sweep.
        // Do not cite it as a latency fix -- the measurement says it is not one.
        //
        // Failure is not fatal: the connection works, and refusing to serve a
        // client over a socket option would be worse than anything it saves.
        if let Err(source) = stream.set_nodelay(true) {
            warn!(?peer_addr, error = %source, "could not disable Nagle on accepted connection");
        }
        debug!(?peer_addr, ?local_addr, "accepted subc TCP connection");
        let router = Arc::clone(&router);
        let auth = auth.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, router, auth).await {
                if err.is_quiet_reject() {
                    debug!(?peer_addr, error = %err, "subc TCP connection rejected before routing");
                } else {
                    warn!(?peer_addr, error = %err, "subc connection ended with error");
                }
            }
        });
    }
}

/// Serve all already-bound loopback TCP listeners until one accept loop fails.
pub async fn serve_listeners(
    listeners: Vec<TcpListener>,
    router: Arc<Router>,
    auth: ServerAuth,
) -> Result<(), ServerError> {
    if listeners.is_empty() {
        return Err(ServerError::NoListeners);
    }

    let (tx, mut rx) = mpsc::channel(listeners.len());
    let mut accept_tasks = AbortTasksOnDrop::default();
    for listener in listeners {
        let router = Arc::clone(&router);
        let auth = auth.clone();
        let tx = tx.clone();
        accept_tasks.push(tokio::spawn(async move {
            let result = serve_listener(listener, router, auth).await;
            let _ = tx.send(result).await;
        }));
    }
    drop(tx);

    rx.recv().await.unwrap_or(Ok(()))
}

#[derive(Default)]
struct AbortTasksOnDrop {
    handles: Vec<JoinHandle<()>>,
}

impl AbortTasksOnDrop {
    fn push(&mut self, handle: JoinHandle<()>) {
        self.handles.push(handle);
    }
}

impl Drop for AbortTasksOnDrop {
    fn drop(&mut self) {
        for handle in &self.handles {
            if !handle.is_finished() {
                handle.abort();
            }
        }
    }
}

#[derive(Debug)]
enum ConnectionLoopExit {
    PeerClosed,
    CloseRequested(CloseReason),
}

/// Run the authenticated frame read -> route loop for one connection.
///
/// Every accepted TCP connection must complete the key-auth prelude before any
/// envelope bytes are read by the router. Outbound frames flow through a bounded
/// [`FrameSink`] drained by one writer task. This locks in the streaming-capable
/// sink shape while intentionally keeping inbound dispatch serial: each routed
/// frame is awaited before reading the next one.
pub async fn handle_connection<S>(
    mut stream: S,
    router: Arc<Router>,
    auth: ServerAuth,
) -> Result<(), ConnectionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Over-cap connections WAIT for a handshake slot (bounded by the auth deadline)
    // instead of being reset. A restart herd — every client and supervised child
    // re-dialing the fresh daemon at once — otherwise loses supervised modules to
    // the permit lottery: each reset burns a module restart-budget slot, and a
    // module that treats auth failure as fatal can exhaust its budget into
    // state=failed within the boot window (2026-07-14 aft outage). On loopback
    // with the pre-auth HMAC deadline, a bounded queue is strictly safer than a
    // reset.
    //
    // Pre-auth time is governed by TWO deliberately separate budgets: the queue
    // wait below is bounded by `auth.deadline`, and `authenticate_server` then
    // starts a FRESH `auth.deadline` for the handshake itself. Total pre-auth
    // occupancy per connection is therefore up to 2x the configured deadline.
    // This is intentional, not an accounting slip: charging queue time against
    // the handshake budget would hand a herd-queued supervised module a
    // near-zero handshake window under CPU saturation, recreating the fatal
    // auth-failure -> restart-budget-burn path this queue exists to prevent.
    // On a loopback-only, key-authenticated listener the doubled bound is a
    // per-connection occupancy cost, not a meaningful DoS surface.
    let permit =
        match tokio::time::timeout(auth.deadline, auth.unauthenticated.clone().acquire_owned())
            .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) | Err(_) => {
                let _ = stream.shutdown().await;
                return Err(ConnectionError::UnauthenticatedCapacity);
            }
        };

    let authenticated = authenticate_server(
        &mut stream,
        auth.key.as_ref(),
        &auth.daemon_id,
        auth.daemon_ver.as_ref(),
        auth.deadline,
    )
    .await
    .map_err(ConnectionError::Auth)?;
    drop(permit);

    let mut connection = router.begin_connection();
    let connection_id = connection.id();
    // `authenticated.role` is CLIENT-SUPPLIED and unverified -- the handshake
    // proves possession of the connection key, nothing about who is calling. So
    // this exclusion is safe ONLY while the role decides a reporting question and
    // never an authorization one: the watchdog's own loopback probe would
    // otherwise inflate the very count it exists to sanity-check.
    //
    // The consequence of it being unverified, stated so nobody has to rediscover
    // it: ANY client holding the key can claim this role and omit itself from
    // `connected_clients`. That is a gauge a caller can lie to, and it is
    // acceptable because the gauge informs an operator rather than gating
    // anything. IF A ROLE IS EVER USED TO DECIDE ADMISSION, CAPACITY, OR PRIVILEGE,
    // this stops being safe and the role must be attested rather than declared --
    // the daemon already has the mechanism for that in the spawn-nonce path used
    // for module identity.
    let _connected_client = (authenticated.role != WATCHDOG_CLIENT_ROLE)
        .then(|| auth.connected_clients.open(connection_id));
    let close_receiver = connection.take_close_receiver();
    debug!(
        connection_id = connection_id.get(),
        "subc authenticated connection opened"
    );

    let (read_half, write_half) = tokio::io::split(stream);
    // Authentication is complete before this buffer can read ahead. The only cancellation
    // of an in-progress frame read terminates the connection, so buffered bytes are never
    // stranded before a later read.
    let mut read_half = BufReader::new(read_half);
    let (tx, rx) = mpsc::channel::<crate::router::OutboundFrame>(CONNECTION_EGRESS_BUFFER);
    let mut writer = tokio::spawn(drain_writer(write_half, rx));

    let egress = FrameSink::new(tx);
    let ctx = RouteCtx {
        connection_id,
        egress: egress.clone(),
    };

    let loop_result = connection_loop(&mut read_half, &router, &ctx, close_receiver).await;

    drop(ctx);
    drop(egress);
    drop(connection);

    let close_reason = match &loop_result {
        Ok(ConnectionLoopExit::CloseRequested(reason)) => Some(reason.to_string()),
        Ok(ConnectionLoopExit::PeerClosed) | Err(_) => None,
    };
    let writer_result = if close_reason.is_some() {
        match timeout(CLOSE_DRAIN_GRACE, &mut writer).await {
            Ok(result) => Some(result.map_err(ConnectionError::WriterTask)),
            Err(_) => {
                warn!(
                    connection_id = connection_id.get(),
                    grace = ?CLOSE_DRAIN_GRACE,
                    "connection writer did not drain after close request; aborting writer task"
                );
                writer.abort();
                let _ = writer.await;
                None
            }
        }
    } else {
        Some(writer.await.map_err(ConnectionError::WriterTask))
    };

    let result = if let Some(reason) = close_reason.as_deref() {
        match writer_result {
            Some(Ok(Ok(()))) | None => Ok(()),
            Some(Ok(Err(writer_err))) => {
                debug!(
                    connection_id = connection_id.get(),
                    close_reason = reason,
                    writer_error = %writer_err,
                    "writer failed after requested connection close"
                );
                Ok(())
            }
            Some(Err(join_err)) => {
                warn!(
                    connection_id = connection_id.get(),
                    close_reason = reason,
                    join_error = %join_err,
                    "writer task join failed after requested connection close"
                );
                Ok(())
            }
        }
    } else {
        let writer_result =
            writer_result.expect("writer result is present without a close request");
        match (loop_result, writer_result) {
            (Err(loop_err), Ok(Ok(()))) => Err(loop_err),
            (Err(loop_err), Ok(Err(writer_err))) => {
                warn!(
                    connection_id = connection_id.get(),
                    writer_error = %writer_err,
                    "writer failed while closing after connection error"
                );
                Err(loop_err)
            }
            (Err(loop_err), Err(join_err)) => {
                warn!(
                    connection_id = connection_id.get(),
                    join_error = %join_err,
                    "writer task join failed while closing after connection error"
                );
                Err(loop_err)
            }
            (Ok(ConnectionLoopExit::PeerClosed), Ok(Ok(()))) => Ok(()),
            (Ok(ConnectionLoopExit::PeerClosed), Ok(Err(writer_err))) => {
                Err(ConnectionError::FrameIo(writer_err))
            }
            (Ok(ConnectionLoopExit::PeerClosed), Err(join_err)) => Err(join_err),
            (Ok(ConnectionLoopExit::CloseRequested(_)), _) => {
                unreachable!("close requests are handled before normal writer result matching")
            }
        }
    };

    match &result {
        Ok(()) => {
            if let Some(reason) = close_reason.as_deref() {
                debug!(
                    connection_id = connection_id.get(),
                    close_reason = reason,
                    "subc connection closed by request"
                );
            } else {
                debug!(
                    connection_id = connection_id.get(),
                    "subc connection closed"
                );
            }
        }
        Err(err) => debug!(
            connection_id = connection_id.get(),
            error = %err,
            "subc connection exited with error"
        ),
    }

    result
}

async fn connection_loop<R>(
    read_half: &mut R,
    router: &Router,
    ctx: &RouteCtx,
    mut close_receiver: ConnectionCloseReceiver,
) -> Result<ConnectionLoopExit, ConnectionError>
where
    R: AsyncRead + Unpin,
{
    loop {
        let frame = tokio::select! {
            close = &mut close_receiver => {
                return Ok(ConnectionLoopExit::CloseRequested(close_reason(close)));
            }
            read = read_frame(read_half) => {
                match read.map_err(ConnectionError::FrameIo)? {
                    Some(frame) => frame,
                    None => return Ok(ConnectionLoopExit::PeerClosed),
                }
            }
        };

        let route_result = tokio::select! {
            close = &mut close_receiver => {
                return Ok(ConnectionLoopExit::CloseRequested(close_reason(close)));
            }
            result = router.route_for_connection(ctx, frame) => result,
        };

        if let Err(err) = route_result {
            if let Some(error_frame) = err.to_error_frame() {
                warn!(
                    connection_id = ctx.connection_id.get(),
                    error = %err,
                    "routing failure recovered with ERROR frame"
                );
                let send_result = tokio::select! {
                    close = &mut close_receiver => {
                        return Ok(ConnectionLoopExit::CloseRequested(close_reason(close)));
                    }
                    result = ctx.egress.send(error_frame) => result,
                };
                send_result.map_err(ConnectionError::Router)?;
            } else {
                debug!(
                    connection_id = ctx.connection_id.get(),
                    error = %err,
                    "fatal routing failure"
                );
                return Err(ConnectionError::Router(err));
            }
        }
    }
}

fn close_reason(
    result: Result<CloseReason, tokio::sync::oneshot::error::RecvError>,
) -> CloseReason {
    result.unwrap_or_else(|_| {
        CloseReason::new(
            "close_registry_dropped",
            "connection close registration was dropped without a reason",
        )
    })
}

async fn drain_writer<W>(
    write_half: W,
    mut rx: mpsc::Receiver<crate::router::OutboundFrame>,
) -> Result<(), FrameIoError>
where
    W: AsyncWrite + Unpin,
{
    let mut writer = BufWriter::new(write_half);
    while let Some(outbound) = rx.recv().await {
        write_outbound(&mut writer, outbound).await?;
        while let Ok(outbound) = rx.try_recv() {
            write_outbound(&mut writer, outbound).await?;
        }
        writer.flush().await.map_err(FrameIoError::Io)?;
    }
    writer.flush().await.map_err(FrameIoError::Io)?;
    Ok(())
}

/// Reply-path half of slow-control diagnosis: a channel-0 reply that sat in
/// the writer queue past the threshold is reported with its queue residency
/// and its own write duration separated, because "writer task not scheduled"
/// and "socket write blocked" are different defects and the sum hides which.
/// Data-plane frames are exempt: their latency is the client's own flow
/// control, and logging them would drown the control signal in bulk traffic.
async fn write_outbound<W>(
    writer: &mut BufWriter<W>,
    outbound: crate::router::OutboundFrame,
) -> Result<(), FrameIoError>
where
    W: AsyncWrite + Unpin,
{
    const SLOW_REPLY_QUEUE: Duration = Duration::from_millis(1000);
    let queued = outbound.enqueued_at.elapsed();
    let frame = outbound.frame;
    if frame.header.channel == 0 && queued >= SLOW_REPLY_QUEUE {
        let write_started = std::time::Instant::now();
        let result = write_frame(writer, &frame).await;
        tracing::warn!(
            corr = frame.header.corr,
            queued_ms = queued.as_millis() as u64,
            write_ms = write_started.elapsed().as_millis() as u64,
            "slow control reply write"
        );
        return result;
    }
    write_frame(writer, &frame).await
}

#[derive(Debug)]
pub enum ServerError {
    NoListeners,
    Accept {
        local_addr: Option<SocketAddr>,
        source: io::Error,
    },
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoListeners => write!(f, "no TCP listeners were provided"),
            Self::Accept { local_addr, source } => match local_addr {
                Some(addr) => write!(f, "failed to accept TCP connection on {addr}: {source}"),
                None => write!(f, "failed to accept TCP connection: {source}"),
            },
        }
    }
}

impl Error for ServerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Accept { source, .. } => Some(source),
            Self::NoListeners => None,
        }
    }
}

#[derive(Debug)]
pub enum ConnectionError {
    Auth(AuthError),
    UnauthenticatedCapacity,
    FrameIo(FrameIoError),
    Router(RouterError),
    WriterTask(tokio::task::JoinError),
}

impl ConnectionError {
    fn is_quiet_reject(&self) -> bool {
        matches!(self, Self::Auth(_) | Self::UnauthenticatedCapacity)
    }
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(err) => write!(f, "connection auth failed: {err}"),
            Self::UnauthenticatedCapacity => write!(
                f,
                "too many concurrent unauthenticated subc TCP connections"
            ),
            Self::FrameIo(err) => write!(f, "frame connection error: {err}"),
            Self::Router(err) => write!(f, "router connection error: {err}"),
            Self::WriterTask(err) => write!(f, "connection writer task failed: {err}"),
        }
    }
}

impl Error for ConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Auth(err) => Some(err),
            Self::FrameIo(err) => Some(err),
            Self::Router(err) => Some(err),
            Self::WriterTask(err) => Some(err),
            Self::UnauthenticatedCapacity => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };

    use super::*;
    use subc_protocol::{
        DecodeError, ErrorBody, Flags, FrameType, Priority, HEADER_LEN, PROTOCOL_VERSION,
    };
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, ReadBuf};

    use subc_transport::{authenticate_client, ConnectionInfo, Endpoint, SCHEMA_VERSION};

    use crate::{ControlHandler, EchoBackend, Frame, ReadStage, Registry};

    const TEST_DEADLINE: Duration = Duration::from_secs(2);
    const TEST_DAEMON_VER: &str = "test-subc-server";

    /// Reply-path stamp, slow polarity: a channel-0 reply whose queue residency
    /// exceeds the threshold must produce the slow-control-reply-write WARN with
    /// queued_ms reflecting the residency. Uses a backdated stamp rather than a
    /// real stall so the test is fast and deterministic.
    #[tokio::test]
    async fn stale_queued_control_reply_logs_slow_reply_write() {
        let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::WARN);
        let (tx, rx) = mpsc::channel::<crate::router::OutboundFrame>(4);
        let reply = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Error,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            7,
            serde_json::to_vec(&ErrorBody {
                code: "test".into(),
                message: "reply".into(),
                detail: None,
            })
            .expect("body encodes"),
        )
        .expect("frame builds");
        tx.send(crate::router::OutboundFrame {
            frame: reply,
            enqueued_at: std::time::Instant::now() - Duration::from_millis(1500),
        })
        .await
        .expect("queued");
        drop(tx);
        let (write_half, mut read_half) = duplex(64 * 1024);
        drain_writer(write_half, rx).await.expect("writer drains");
        let mut sink = Vec::new();
        read_half.read_to_end(&mut sink).await.expect("read");
        assert!(!sink.is_empty(), "frame reached the socket");
        let captured = crate::router::test_log::captured_logs(&logs);
        assert!(
            captured.contains("slow control reply write") && captured.contains("corr=7"),
            "expected slow reply WARN naming corr, got: {captured}"
        );
        let queued_ms: u64 = captured
            .split("queued_ms=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .expect("queued_ms present");
        assert!(
            queued_ms >= 1500,
            "queued_ms reflects residency: {queued_ms}"
        );
    }

    /// Fast polarity: a promptly-drained control reply and a stale DATA-PLANE
    /// frame must both stay silent — the WARN is channel-0-only by design, and
    /// a healthy queue must add zero log volume.
    #[tokio::test]
    async fn fresh_control_and_stale_data_frames_log_nothing() {
        let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::WARN);
        let (tx, rx) = mpsc::channel::<crate::router::OutboundFrame>(4);
        let control = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Error,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            8,
            serde_json::to_vec(&ErrorBody {
                code: "test".into(),
                message: "fresh".into(),
                detail: None,
            })
            .expect("body encodes"),
        )
        .expect("frame builds");
        tx.send(crate::router::OutboundFrame {
            frame: control,
            enqueued_at: std::time::Instant::now(),
        })
        .await
        .expect("queued");
        let data = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Error,
            Flags::new(false, Priority::Interactive, false),
            9,
            1,
            9,
            serde_json::to_vec(&ErrorBody {
                code: "test".into(),
                message: "data".into(),
                detail: None,
            })
            .expect("body encodes"),
        )
        .expect("frame builds");
        tx.send(crate::router::OutboundFrame {
            frame: data,
            enqueued_at: std::time::Instant::now() - Duration::from_millis(5000),
        })
        .await
        .expect("queued");
        drop(tx);
        let (write_half, _read_half) = duplex(64 * 1024);
        drain_writer(write_half, rx).await.expect("writer drains");
        let captured = crate::router::test_log::captured_logs(&logs);
        assert!(
            !captured.contains("slow control reply write"),
            "no WARN for fresh control or stale data frames, got: {captured}"
        );
    }

    struct CountingReader {
        bytes: Vec<u8>,
        offset: usize,
        first_read_end: Option<usize>,
        reads: Arc<AtomicUsize>,
    }

    impl CountingReader {
        fn new(bytes: Vec<u8>, first_read_end: Option<usize>) -> (Self, Arc<AtomicUsize>) {
            let reads = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    bytes,
                    offset: 0,
                    first_read_end,
                    reads: Arc::clone(&reads),
                },
                reads,
            )
        }
    }

    impl AsyncRead for CountingReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let available = self.bytes.len().saturating_sub(self.offset);
            let first_read_remaining = self
                .first_read_end
                .filter(|end| self.offset < *end)
                .map_or(available, |end| end - self.offset);
            let count = available.min(first_read_remaining).min(buf.remaining());
            let end = self.offset + count;
            buf.put_slice(&self.bytes[self.offset..end]);
            self.offset = end;
            Poll::Ready(Ok(()))
        }
    }

    fn encode_frames(frames: &[Frame]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for frame in frames {
            bytes.extend_from_slice(&frame.header.encode());
            bytes.extend_from_slice(&frame.body);
        }
        bytes
    }

    async fn read_frames<R>(reader: &mut R, count: usize) -> Vec<Frame>
    where
        R: AsyncRead + Unpin,
    {
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            frames.push(read_frame(reader).await.unwrap().unwrap());
        }
        frames
    }

    fn request(channel: u16, corr: u64, body: &[u8]) -> Frame {
        Frame::build(
            FrameType::Request,
            Flags::new(true, Priority::Interactive, false),
            channel,
            0,
            corr,
            body.to_vec(),
        )
        .unwrap()
    }

    fn echo_router() -> Arc<Router> {
        let mut router = Router::with_default_self_handler();
        router.register_backend(7, EchoBackend).unwrap();
        router.register_backend(9, EchoBackend).unwrap();
        Arc::new(router)
    }

    fn test_auth() -> (ServerAuth, ConnectionInfo) {
        test_auth_with_limit(4)
    }

    fn test_auth_with_limit(max_unauthenticated: usize) -> (ServerAuth, ConnectionInfo) {
        let key = vec![0x42; 32];
        let daemon_id = [0x24; 16];
        let conn = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_owned(),
                port: 1,
            }],
            key: key.clone(),
            daemon_id,
            pid: std::process::id(),
            daemon_ver: TEST_DAEMON_VER.to_owned(),
        };
        (
            ServerAuth::with_limits(
                key,
                daemon_id,
                TEST_DAEMON_VER,
                TEST_DEADLINE,
                max_unauthenticated,
            ),
            conn,
        )
    }

    async fn authenticate<S>(stream: &mut S, conn: &ConnectionInfo)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        authenticate_client(stream, conn, TEST_DEADLINE)
            .await
            .expect("test client should authenticate")
    }

    #[tokio::test]
    async fn buffered_frame_reader_coalesces_reads_and_preserves_short_reads() {
        let frames = vec![
            request(7, 1, b"first"),
            request(9, 2, b"second"),
            request(7, 3, b"third"),
            request(9, 4, b"fourth"),
        ];
        let bytes = encode_frames(&frames);

        let (mut direct, direct_reads) = CountingReader::new(bytes.clone(), None);
        assert_eq!(read_frames(&mut direct, frames.len()).await, frames);
        assert_eq!(direct_reads.load(Ordering::Relaxed), frames.len() * 3);

        let (buffered_source, buffered_reads) = CountingReader::new(bytes, None);
        let mut buffered = BufReader::new(buffered_source);
        assert_eq!(read_frames(&mut buffered, frames.len()).await, frames);
        assert_eq!(buffered_reads.load(Ordering::Relaxed), 1);

        let split_frame = request(7, 5, b"split-body");
        let split_bytes = encode_frames(std::slice::from_ref(&split_frame));
        let (split_source, split_reads) = CountingReader::new(split_bytes, Some(10));
        let mut split_reader = BufReader::new(split_source);
        assert_eq!(
            read_frame(&mut split_reader).await.unwrap(),
            Some(split_frame)
        );
        assert_eq!(split_reads.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn interleaved_channels_on_one_stream_demux_byte_identically_after_auth() {
        let (mut client, server_stream) = duplex(4096);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(server_stream, echo_router(), auth));
        authenticate(&mut client, &conn).await;
        let frames = [
            request(7, 1, b"chan7-first\0opaque"),
            request(9, 2, b"chan9-middle-{json?}"),
            request(7, 3, b"chan7-second\xffbytes"),
        ];

        for frame in &frames {
            crate::write_frame(&mut client, frame).await.unwrap();
        }

        for expected in &frames {
            let response = crate::read_frame(&mut client).await.unwrap().unwrap();
            assert_eq!(response.header.ty, FrameType::Response);
            assert_eq!(response.header.channel, expected.header.channel);
            assert_eq!(response.header.corr, expected.header.corr);
            assert_eq!(response.body, expected.body);
        }

        drop(client);
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn channel_zero_goes_to_subc_self_handler_after_auth() {
        let (mut client, server_stream) = duplex(512);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(
            server_stream,
            Arc::new(Router::with_default_self_handler()),
            auth,
        ));
        authenticate(&mut client, &conn).await;
        let ping = Frame::build(
            FrameType::Ping,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            55,
            Vec::new(),
        )
        .unwrap();

        crate::write_frame(&mut client, &ping).await.unwrap();
        let response = crate::read_frame(&mut client).await.unwrap().unwrap();

        assert_eq!(response.header.ty, FrameType::Pong);
        assert_eq!(response.header.channel, 0);
        assert_eq!(response.header.corr, 55);
        assert!(response.body.is_empty());

        drop(client);
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unauthenticated_connection_is_rejected_before_routing() {
        let (mut client, server_stream) = duplex(512);
        let (auth, _conn) = test_auth();
        let registry = Arc::new(Registry::default());
        let router = Arc::new(Router::with_control_handler(Arc::new(ControlHandler::new(
            Arc::clone(&registry),
        ))));
        let server = tokio::spawn(handle_connection(server_stream, router, auth));
        let ping = Frame::build(
            FrameType::Ping,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            66,
            Vec::new(),
        )
        .unwrap();

        crate::write_frame(&mut client, &ping).await.unwrap();
        if let Ok(Ok(Some(frame))) =
            tokio::time::timeout(Duration::from_millis(200), crate::read_frame(&mut client)).await
        {
            panic!("unauthenticated frame reached router: {frame:?}");
        }

        let err = server.await.unwrap().unwrap_err();
        assert!(matches!(err, ConnectionError::Auth(_)));
        assert_eq!(registry.active_registration_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn over_cap_peer_queues_for_a_slot_and_authenticates_when_one_frees() {
        // Restart-herd contract: an over-cap pre-auth connection WAITS (bounded by
        // the auth deadline) instead of being reset. When the slot-holder finishes,
        // the queued peer must complete a normal handshake — the 2026-07-14 aft
        // outage was exactly a supervised child being reset out of this lottery.
        let (mut first_client, first_server_stream) = duplex(2048);
        let (mut second_client, second_server_stream) = duplex(2048);
        let (auth, conn) = test_auth_with_limit(1);
        let registry = Arc::new(Registry::default());
        let router = Arc::new(Router::with_control_handler(Arc::new(ControlHandler::new(
            Arc::clone(&registry),
        ))));

        let first_server = tokio::spawn(handle_connection(
            first_server_stream,
            Arc::clone(&router),
            auth.clone(),
        ));

        let second_server = tokio::spawn(handle_connection(
            second_server_stream,
            Arc::clone(&router),
            auth.clone(),
        ));

        // The second peer must still be pending (not reset) while the slot is held.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !second_server.is_finished(),
            "queued peer must not be reset"
        );

        // First peer completes its handshake, freeing the slot; the queued second
        // peer then authenticates normally.
        authenticate(&mut first_client, &conn).await;
        authenticate(&mut second_client, &conn).await;

        drop(first_client);
        drop(second_client);
        let _ = first_server.await;
        let _ = second_server.await;
        assert_eq!(registry.active_registration_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn over_cap_peer_is_rejected_when_no_slot_frees_within_deadline() {
        // The deadline stays the DoS bound: if no slot frees, the queued peer is
        // rejected at the deadline with a closed stream. The slot is held DIRECTLY
        // (not by another connection) so nothing can free it mid-test — a squatting
        // connection's own auth deadline would release the slot at exactly the
        // waiter's timeout, making the outcome racy.
        let (mut second_client, second_server_stream) = duplex(512);
        let (auth, conn) = test_auth_with_limit(1);
        let registry = Arc::new(Registry::default());
        let router = Arc::new(Router::with_control_handler(Arc::new(ControlHandler::new(
            Arc::clone(&registry),
        ))));

        let held_slot = auth
            .unauthenticated
            .clone()
            .try_acquire_owned()
            .expect("sole pre-auth slot");

        let second_server = tokio::spawn(handle_connection(
            second_server_stream,
            Arc::clone(&router),
            auth.clone(),
        ));
        let second_err = tokio::time::timeout(TEST_DEADLINE * 2, second_server)
            .await
            .expect("capacity reject should settle at the deadline")
            .expect("second connection task should not panic")
            .expect_err("queued peer must be rejected when no slot frees");
        drop(held_slot);
        assert!(matches!(
            second_err,
            ConnectionError::UnauthenticatedCapacity
        ));
        let mut closed = [0u8; 1];
        assert_eq!(
            second_client.read(&mut closed).await.unwrap(),
            0,
            "capacity-rejected peer should observe a closed stream"
        );
        assert_eq!(registry.active_registration_count().unwrap(), 0);

        let (mut authed_client, authed_server_stream) = duplex(2048);
        let authed_server = tokio::spawn(handle_connection(
            authed_server_stream,
            Arc::clone(&router),
            auth,
        ));
        authenticate(&mut authed_client, &conn).await;
        let ping = Frame::build(
            FrameType::Ping,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            77,
            Vec::new(),
        )
        .unwrap();
        crate::write_frame(&mut authed_client, &ping).await.unwrap();
        let pong = crate::read_frame(&mut authed_client)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pong.header.ty, FrameType::Pong);
        assert_eq!(pong.header.channel, 0);
        assert_eq!(pong.header.corr, 77);
        assert_eq!(registry.active_registration_count().unwrap(), 0);

        drop(authed_client);
        authed_server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn serve_listeners_with_no_listeners_returns_typed_error() {
        let (auth, _conn) = test_auth();
        let err = serve_listeners(Vec::new(), echo_router(), auth)
            .await
            .expect_err("empty listener set must fail loudly");
        assert!(matches!(err, ServerError::NoListeners));
    }

    #[tokio::test]
    async fn malformed_header_returns_typed_error_no_panic() {
        let (mut client, server_stream) = duplex(128);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(server_stream, echo_router(), auth));
        authenticate(&mut client, &conn).await;
        let mut header = [0u8; HEADER_LEN];
        header[4] = PROTOCOL_VERSION;
        header[5] = 250;

        client.write_all(&header).await.unwrap();
        drop(client);

        let err = server.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            ConnectionError::FrameIo(FrameIoError::DecodeHeader(DecodeError::UnknownFrameType {
                byte: 250
            }))
        ));
    }

    #[tokio::test]
    async fn truncated_body_returns_typed_error_no_panic() {
        let (mut client, server_stream) = duplex(128);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(server_stream, echo_router(), auth));
        authenticate(&mut client, &conn).await;
        let frame = request(7, 8, b"abcd");

        client.write_all(&frame.header.encode()).await.unwrap();
        client.write_all(b"ab").await.unwrap();
        drop(client);

        let err = server.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            ConnectionError::FrameIo(FrameIoError::UnexpectedEof {
                stage: ReadStage::Body,
                expected: 4,
                actual: 2
            })
        ));
    }

    #[tokio::test]
    async fn unknown_channel_is_returned_as_error_frame_and_connection_continues() {
        let (mut client, server_stream) = duplex(1024);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(server_stream, echo_router(), auth));
        authenticate(&mut client, &conn).await;
        let unknown = request(42, 10, b"lost");
        let known = request(7, 11, b"still-routes");

        crate::write_frame(&mut client, &unknown).await.unwrap();
        crate::write_frame(&mut client, &known).await.unwrap();

        let error = crate::read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(error.header.ty, FrameType::Error);
        assert_eq!(error.header.channel, 42);
        assert_eq!(error.header.corr, 10);
        let error_body: ErrorBody = serde_json::from_slice(&error.body).unwrap();
        assert_eq!(error_body.code, "unknown_channel");

        let response = crate::read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(response.header.ty, FrameType::Response);
        assert_eq!(response.header.channel, 7);
        assert_eq!(response.header.corr, 11);
        assert_eq!(response.body, b"still-routes");

        drop(client);
        server.await.unwrap().unwrap();
    }
}
