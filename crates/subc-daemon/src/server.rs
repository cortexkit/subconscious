use std::{
    error::Error,
    fmt, io,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use subc_transport::{authenticate_server, AuthError, DAEMON_ID_LEN, WATCHDOG_CLIENT_ROLE};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    net::TcpListener,
    sync::{mpsc, Semaphore},
    task::{JoinHandle, JoinSet},
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

/// Bytes (envelope header plus body) one connection's egress queue may hold
/// before a non-waiting enqueue is refused, which closes that connection.
///
/// Sized for a client that is slow but still reading: one connection can
/// multiplex many token streams, and at roughly 200-byte frames (21-byte header
/// plus body) 4 MiB is about 19,000 queued frames, i.e. thousands of frames on
/// each of several routes, enough to ride out a pause of seconds. It is also the
/// most a stuck client can make the daemon hold for it: 4 MiB per connection,
/// so even 128 stuck connections stay near 512 MiB rather than growing without
/// bound. A single frame larger than the whole budget is still admitted into an
/// empty queue so it can be sent at all.
pub const CONNECTION_EGRESS_BYTE_BUDGET: usize = 4 * 1024 * 1024;
/// Frame-count backstop for the same queue (the tokio channel's capacity).
/// The byte budget is the real bound; this one only matters for floods of tiny
/// or empty frames, which cost bookkeeping per frame rather than bytes. 32,768
/// frames times 128 bytes is exactly the 4 MiB budget, so for any mean frame of
/// 128 bytes or more the byte budget binds first; below that (for example
/// 21-byte empty frames, of which 4 MiB would be almost 200,000) the count
/// does. The channel allocates slots lazily, so a large capacity costs nothing
/// until it is used.
pub const CONNECTION_EGRESS_FRAME_CAP: usize = 32 * 1024;
/// A pending `route.open` holds one reserved slot in the connection's egress
/// queue until its module answers, and its response is sent outside the byte
/// budget. Eight concurrent opens per connection keep a reconnect burst
/// parallel while bounding both the reserved slots and the bytes that bypass
/// the budget.
pub const MAX_PENDING_ROUTE_OPENS_PER_CONNECTION: usize = 8;
/// A reconnect herd may spread one target's opens over many client connections.
/// Two safe per-connection bursts retain useful parallelism without restoring
/// the hundreds-of-binds fanout that serial dispatch used to suppress.
pub(crate) const MAX_PENDING_ROUTE_BINDS_PER_TARGET: usize =
    MAX_PENDING_ROUTE_OPENS_PER_CONNECTION * 2;
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
    serve_listener_with_accept(listener.local_addr().ok(), router, auth, || {
        listener.accept()
    })
    .await
}

async fn serve_listener_with_accept<A, F>(
    local_addr: Option<SocketAddr>,
    router: Arc<Router>,
    auth: ServerAuth,
    mut accept: A,
) -> Result<(), ServerError>
where
    A: FnMut() -> F,
    F: std::future::Future<Output = io::Result<(tokio::net::TcpStream, SocketAddr)>>,
{
    loop {
        let (stream, peer_addr) = match accept().await {
            Ok(accepted) => accepted,
            Err(source) => {
                let kind = source.kind();
                // Aborted/reset connections and interrupted syscalls affect one accept only.
                // File-descriptor or socket-buffer exhaustion needs a short backoff to avoid spinning;
                // other errors may mean the listener itself is unusable.
                let exhausted = accept_resource_exhausted(&source);
                if matches!(
                    kind,
                    io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::Interrupted
                ) || exhausted
                {
                    warn!(?local_addr, error = %source, "temporary TCP accept failure");
                    if exhausted {
                        tokio::time::sleep(Duration::from_millis(75)).await;
                    }
                    continue;
                }
                return Err(ServerError::Accept { local_addr, source });
            }
        };
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

fn accept_resource_exhausted(error: &io::Error) -> bool {
    // ENFILE/EMFILE/ENOBUFS by name rather than number: the numbers differ by
    // platform (ENOBUFS is 55 on macOS and 105 on Linux, and each number means
    // something unrelated on the other), so a literal list misclassifies.
    // Winsock: WSAEMFILE and WSAENOBUFS.
    #[cfg(unix)]
    {
        use rustix::io::Errno;
        let exhausted = [Errno::NFILE, Errno::MFILE, Errno::NOBUFS];
        error
            .raw_os_error()
            .is_some_and(|code| exhausted.iter().any(|e| e.raw_os_error() == code))
    }
    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(10024 | 10055))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
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
/// [`FrameSink`] drained by one writer task. Inbound dispatch remains serial for
/// every frame except `route.open`; only that bind wait runs in a connection-owned
/// task so it cannot hold unrelated frames behind a slow target module.
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
    // Authentication is complete before this buffer can read ahead. The connection loop
    // retains a partial frame read across completed route.open tasks; dropping the read
    // happens only when a frame finishes or the connection ends.
    let mut read_half = BufReader::new(read_half);
    let (egress, rx) = connection_egress();
    let mut writer = tokio::spawn(drain_writer(write_half, rx));

    let ctx = RouteCtx {
        connection_id,
        egress: egress.clone(),
    };

    let mut route_open_tasks = JoinSet::new();
    let loop_result = connection_loop(
        &mut read_half,
        Arc::clone(&router),
        ctx.clone(),
        close_receiver,
        &mut route_open_tasks,
    )
    .await;

    // Every in-flight route.open must have FINISHED before `connection` drops,
    // because that drop runs the forwarding cleanup for this id, and cleanup is
    // also where the id's closing mark is lifted. Dropping a JoinSet only
    // requests abort: a task already running on another worker keeps going
    // until its next await, and could commit a route for this connection after
    // cleanup has cleared it, with nothing left to refuse it and nothing to
    // clean it up. shutdown() aborts and then waits.
    route_open_tasks.shutdown().await;

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
    router: Arc<Router>,
    ctx: RouteCtx,
    mut close_receiver: ConnectionCloseReceiver,
    route_open_tasks: &mut JoinSet<Result<(), RouterError>>,
) -> Result<ConnectionLoopExit, ConnectionError>
where
    R: AsyncRead + Unpin,
{
    loop {
        while let Some(result) = route_open_tasks.try_join_next() {
            finish_route_open_task(result)?;
        }

        // Keep the same read future when a route.open completes: read_frame owns
        // partial header/body buffers that would be lost if that future were dropped.
        let read = read_frame(&mut *read_half);
        tokio::pin!(read);
        let frame = loop {
            tokio::select! {
                close = &mut close_receiver => {
                    return Ok(ConnectionLoopExit::CloseRequested(close_reason(close)));
                }
                result = route_open_tasks.join_next(), if !route_open_tasks.is_empty() => {
                    finish_route_open_task(
                        result.expect("a non-empty route.open JoinSet has a next task")
                    )?;
                }
                result = &mut read => {
                    break match result.map_err(ConnectionError::FrameIo)? {
                        Some(frame) => frame,
                        None => return Ok(ConnectionLoopExit::PeerClosed),
                    };
                }
            }
        };

        if let Some(target_module_id) = router.route_open_target(&frame) {
            // A task can finish while the reader is waiting for the next frame.
            // Reap it before checking admission so completed work never occupies
            // one of the deliberately scarce connection slots.
            while let Some(result) = route_open_tasks.try_join_next() {
                finish_route_open_task(result)?;
            }

            if route_open_tasks.len() >= MAX_PENDING_ROUTE_OPENS_PER_CONNECTION {
                // Never wait for capacity here: doing so would recreate the same
                // reader head-of-line stall with a smaller constant.
                let refusal = router
                    .route_open_capacity_refusal(
                        &ctx,
                        &frame,
                        &target_module_id,
                        route_open_tasks.len(),
                        MAX_PENDING_ROUTE_OPENS_PER_CONNECTION,
                    )
                    .map_err(ConnectionError::Router)?;
                let send_result = tokio::select! {
                    close = &mut close_receiver => {
                        return Ok(ConnectionLoopExit::CloseRequested(close_reason(close)));
                    }
                    result = ctx.egress.send(refusal) => result,
                };
                send_result.map_err(ConnectionError::Router)?;
                continue;
            }

            let task_router = Arc::clone(&router);
            let task_ctx = ctx.clone();
            // Start slow-dispatch timing before spawn so it covers the task's
            // full scheduler and handler lifetime, as inline timing did.
            let dispatch_started_at = Instant::now();
            route_open_tasks.spawn(async move {
                route_open_tail(task_router, task_ctx, frame, dispatch_started_at).await
            });
            continue;
        }

        // Every non-route.open frame keeps the original serial dispatch path,
        // including read backpressure and close-cancellation coupling.
        let route_result = tokio::select! {
            close = &mut close_receiver => {
                return Ok(ConnectionLoopExit::CloseRequested(close_reason(close)));
            }
            result = router.route_for_connection(&ctx, frame) => result,
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

async fn route_open_tail(
    router: Arc<Router>,
    ctx: RouteCtx,
    frame: crate::Frame,
    dispatch_started_at: Instant,
) -> Result<(), RouterError> {
    match router
        .route_for_connection_started(&ctx, frame, Some(dispatch_started_at))
        .await
    {
        Ok(()) => Ok(()),
        Err(err) => {
            let Some(error_frame) = err.to_error_frame() else {
                return Err(err);
            };
            warn!(
                connection_id = ctx.connection_id.get(),
                error = %err,
                "routing failure recovered with ERROR frame"
            );
            ctx.egress.send(error_frame).await
        }
    }
}

fn finish_route_open_task(
    result: Result<Result<(), RouterError>, tokio::task::JoinError>,
) -> Result<(), ConnectionError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(ConnectionError::Router(err)),
        Err(err) => Err(ConnectionError::Router(RouterError::backend(
            0,
            0,
            format!("route.open task failed: {err}"),
        ))),
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

/// The outbound queue for one daemon connection: a sink bounded by
/// [`CONNECTION_EGRESS_BYTE_BUDGET`] and [`CONNECTION_EGRESS_FRAME_CAP`], and
/// the receiver its writer drains.
pub(crate) fn connection_egress() -> (FrameSink, mpsc::Receiver<crate::router::OutboundFrame>) {
    let (tx, rx) = mpsc::channel::<crate::router::OutboundFrame>(CONNECTION_EGRESS_FRAME_CAP);
    (
        FrameSink::with_byte_budget(tx, CONNECTION_EGRESS_BYTE_BUDGET),
        rx,
    )
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
    // Held until this frame has been written, then dropped at the end of this
    // function, which gives its bytes back to the connection's egress budget.
    let charge = outbound.charge;
    if let Some(charge) = &charge {
        charge.taken_by_writer();
    }
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
        result?;
    } else {
        write_frame(writer, &frame).await?;
    }
    if let Some(flushed) = outbound.flushed {
        writer.flush().await.map_err(FrameIoError::Io)?;
        let _ = flushed.send(());
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn shutdown_notice_ack_waits_for_socket_flush() {
    let (socket, mut peer) = tokio::io::duplex(1);
    let (tx, rx) = mpsc::channel(1);
    let sink = FrameSink::new(tx);
    let writer = tokio::spawn(drain_writer(socket, rx));
    let frame = crate::Frame::build(
        subc_protocol::FrameType::Push,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
        0,
        0,
        0,
        b"notice".to_vec(),
    )
    .unwrap();
    let send = sink.send_flushed(frame);
    tokio::pin!(send);
    assert!(
        timeout(Duration::from_millis(20), &mut send).await.is_err(),
        "queueing bytes is not a socket flush acknowledgement"
    );
    let (sent, received) = timeout(Duration::from_secs(1), async {
        tokio::join!(&mut send, read_frame(&mut peer))
    })
    .await
    .unwrap();
    sent.unwrap();
    assert_eq!(received.unwrap().unwrap().body, b"notice");
    writer.abort();
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
            flushed: None,
            charge: None,
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
            flushed: None,
            charge: None,
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
            flushed: None,
            charge: None,
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
    async fn bind_ack_during_partial_frame_preserves_next_request() {
        use subc_control::ClientControlRequest;
        use subc_protocol::{
            manifest::{
                Concurrency, ExecutionMode, IdentityScope, ModuleManifest, ProviderRole, Tool,
            },
            session::{ModuleControlRequest, ModuleControlResponse},
            BindIdentity, ModuleHelloBody, RouteTarget,
        };

        let mut configured_router = Router::with_default_self_handler();
        configured_router.register_backend(7, EchoBackend).unwrap();
        let router = Arc::new(configured_router);
        let (auth, conn) = test_auth();
        let (mut module, module_stream) = duplex(4096);
        let module_server = tokio::spawn(handle_connection(
            module_stream,
            Arc::clone(&router),
            auth.clone(),
        ));
        authenticate(&mut module, &conn).await;
        let manifest = ModuleManifest::builder("frame-test", "0.1.0")
            .protocol_ver(PROTOCOL_VERSION)
            .provides(vec![ProviderRole::ToolProvider {
                tools: vec![Tool {
                    name: "read".into(),
                    description: None,
                    execution_mode: ExecutionMode::Pure,
                    schema: serde_json::json!({"type": "object"}),
                }],
                identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
                concurrency: Concurrency::ModuleManaged,
                emits_push: true,
                sub_supervises: true,
            }])
            .build();
        let hello = Frame::build(
            FrameType::Hello,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&ModuleHelloBody {
                manifest,
                protocol_ver: PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: None,
            })
            .unwrap(),
        )
        .unwrap();
        crate::write_frame(&mut module, &hello).await.unwrap();
        assert_eq!(
            read_frame(&mut module).await.unwrap().unwrap().header.ty,
            FrameType::HelloAck
        );

        let (mut client, client_stream) = duplex(4096);
        let client_server = tokio::spawn(handle_connection(client_stream, router, auth));
        authenticate(&mut client, &conn).await;
        let open = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            2,
            serde_json::to_vec(&ClientControlRequest::RouteOpen {
                target: RouteTarget::ToolProvider {
                    module_id: "frame-test".into(),
                },
                identity: BindIdentity::new(std::env::current_dir().unwrap(), "unit", "session"),
                consumer_identity: None,
                consumer_capabilities: None,
                admission_facts: None,
            })
            .unwrap(),
        )
        .unwrap();
        crate::write_frame(&mut client, &open).await.unwrap();
        let bind = timeout(TEST_DEADLINE, read_frame(&mut module))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            serde_json::from_slice::<ModuleControlRequest>(&bind.body).unwrap(),
            ModuleControlRequest::RouteBind { .. }
        ));

        let ping = request(7, 3, b"partial-frame-body");
        client.write_all(&ping.header.encode()).await.unwrap();
        // Give the connection reader time to consume the header while the bind is pending.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let ack = Frame::build(
            FrameType::Response,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            bind.header.corr,
            serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap(),
        )
        .unwrap();
        crate::write_frame(&mut module, &ack).await.unwrap();
        let opened = timeout(TEST_DEADLINE, read_frame(&mut client))
            .await
            .unwrap()
            .unwrap();
        if opened.is_none() {
            panic!("client closed: {:?}", client_server.await);
        }
        let opened = opened.unwrap();
        assert_eq!(opened.header.corr, 2);
        client.write_all(&ping.body).await.unwrap();
        let pong = timeout(TEST_DEADLINE, read_frame(&mut client))
            .await
            .expect("partial frame must reach the router after the bind ack")
            .expect("frame must decode")
            .expect("connection must remain open");
        assert_eq!(pong.header.ty, FrameType::Response);
        assert_eq!(pong.header.corr, 3);
        assert_eq!(pong.body, ping.body);
        drop(client);
        drop(module);
        client_server.await.unwrap().unwrap();
        let _ = module_server.await.unwrap();
    }

    #[tokio::test]
    async fn aborted_accept_does_not_end_listener() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let addr = listener.local_addr().unwrap();
        let (auth, conn) = test_auth();
        let mut attempts = 0;
        let server = tokio::spawn(serve_listener_with_accept(
            Some(addr),
            echo_router(),
            auth,
            move || {
                attempts += 1;
                let result = if attempts == 1 {
                    Some(io::Error::from(io::ErrorKind::ConnectionAborted))
                } else {
                    None
                };
                let listener = Arc::clone(&listener);
                async move {
                    match result {
                        Some(err) => Err(err),
                        None => listener.accept().await,
                    }
                }
            },
        ));
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        authenticate(&mut client, &conn).await;
        let ping = Frame::build(
            FrameType::Ping,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            77,
            Vec::new(),
        )
        .unwrap();
        crate::write_frame(&mut client, &ping).await.unwrap();
        let pong = timeout(TEST_DEADLINE, read_frame(&mut client))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(pong.header.ty, FrameType::Pong);
        assert!(
            !server.is_finished(),
            "a temporary accept failure must not stop the listener"
        );
        server.abort();
    }

    #[tokio::test]
    async fn exhausted_accept_backs_off_then_serves_connection() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let addr = listener.local_addr().unwrap();
        let (auth, conn) = test_auth();
        let mut attempts = 0;
        let server = tokio::spawn(serve_listener_with_accept(
            Some(addr),
            echo_router(),
            auth,
            move || {
                attempts += 1;
                // The platform's "too many open files" code: EMFILE on Unix,
                // WSAEMFILE on Windows. A bare 24 is ERROR_BAD_LENGTH on Windows.
                #[cfg(unix)]
                let emfile = rustix::io::Errno::MFILE.raw_os_error();
                #[cfg(windows)]
                let emfile = 10024;
                let error = (attempts == 1).then(|| io::Error::from_raw_os_error(emfile));
                let listener = Arc::clone(&listener);
                async move {
                    match error {
                        Some(err) => Err(err),
                        None => listener.accept().await,
                    }
                }
            },
        ));
        let started = Instant::now();
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        authenticate(&mut client, &conn).await;
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "fd exhaustion must back off before retrying"
        );
        assert!(!server.is_finished());
        server.abort();
    }

    #[tokio::test]
    async fn fatal_accept_error_still_stops_listener() {
        let (auth, _) = test_auth();
        let err = serve_listener_with_accept(None, echo_router(), auth, || async {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .await
        .unwrap_err();
        assert!(
            matches!(err, ServerError::Accept { source, .. } if source.kind() == io::ErrorKind::PermissionDenied)
        );
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
