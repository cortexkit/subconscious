//! Module connection generations and in-place upgrade lifecycle.

use super::*;
use std::sync::OnceLock;
use subc_protocol::session::LIVE_UPGRADE_CAPABILITY;

/// Complete scope set to send first on a resumed connection.
#[derive(Debug, Clone)]
pub struct ResumeScopeSync {
    /// The full-sync generation, as in [`ModuleHandle::scope_sync`].
    pub generation: u64,
    /// The scope records owned by this module.
    pub scopes: Vec<ScopeRecord>,
}

pub(super) struct Generations {
    current: Mutex<Arc<ModuleHandleShared>>,
    metadata: AckMetadata,
}

pub(super) struct AckMetadata {
    pub(super) machine_id: Option<MachineId>,
    pub(super) capabilities: Vec<String>,
    pub(super) storage: Option<serde_json::Value>,
    next: OnceLock<Box<Self>>,
}

impl AckMetadata {
    fn new(ack: &ModuleHelloAckBody) -> Self {
        Self {
            machine_id: ack
                .machine_id
                .as_deref()
                .and_then(|id| MachineId::parse(id).ok()),
            capabilities: ack.subc_capabilities.clone(),
            storage: ack.storage.clone(),
            next: OnceLock::new(),
        }
    }

    fn latest(&self) -> &Self {
        let mut metadata = self;
        while let Some(next) = metadata.next.get() {
            metadata = next;
        }
        metadata
    }
}

impl Generations {
    pub(super) fn new(ack: &ModuleHelloAckBody, current: Arc<ModuleHandleShared>) -> Self {
        Self {
            current: Mutex::new(current),
            metadata: AckMetadata::new(ack),
        }
    }

    pub(super) fn metadata(&self) -> &AckMetadata {
        // Publish the metadata and the live connection under the same lock.
        // Immutable metadata stays alive so public borrowed accessors keep
        // working even if the caller holds a reference across an upgrade.
        let _current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        self.metadata.latest()
    }

    fn install(&self, ack: &ModuleHelloAckBody, connection: ModuleHandleShared) {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        let installed = self
            .metadata
            .latest()
            .next
            .set(Box::new(AckMetadata::new(ack)));
        assert!(installed.is_ok(), "serialized ACK metadata installation");
        *current = Arc::new(connection);
    }
}

impl ModuleHandle {
    pub(super) fn shared(&self) -> Arc<ModuleHandleShared> {
        Arc::clone(
            &self
                .generations
                .current
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    pub(super) fn announce_upgrade(&self, reconnect_by_ms: u64) {
        let shared = self.shared();
        let mut state = shared.lock_inner();
        if state.upgrade.manifest.is_some() && state.upgrade.deadline.is_none() {
            let remaining = drain_deadline(reconnect_by_ms)
                .duration_since(SystemTime::now())
                .unwrap_or_default();
            let now = Instant::now();
            state.upgrade.deadline = Some(now.checked_add(remaining).unwrap_or(now));
            shared.upgrade_notice.cancel();
        }
    }
}

#[derive(Default)]
pub(super) struct UpgradeState {
    manifest: Option<ModuleManifest>,
    resume_token: Option<String>,
    deadline: Option<Instant>,
    starting: bool,
}

impl UpgradeState {
    pub(super) fn fenced(&self) -> bool {
        self.starting || self.deadline.is_some()
    }

    pub(super) fn update_manifest(&mut self, update: impl FnOnce(&mut ModuleManifest)) {
        if let Some(manifest) = &mut self.manifest {
            update(manifest);
        }
    }
}

pub(super) fn upgrade_refusal() -> ErrorBody {
    ErrorBody::module_reloading_for_daemon_upgrade(
        "daemon upgrade in progress; retry after reconnecting",
    )
}

impl ServeOptions {
    /// Allow a supervised primary connection to resume after an announced
    /// daemon upgrade. Off by default; available on macOS and Linux only.
    /// Opt in only if [`ModuleHandler::on_reconnected`] rebuilds all state
    /// tied to the previous connection, including subscriptions and routes.
    pub fn with_live_upgrade(mut self, enabled: bool) -> Self {
        self.live_upgrade = enabled;
        self
    }
}

fn advertises_upgrade(options: &ServeOptions, has_launch_nonce: bool) -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
        && options.live_upgrade
        && !options.prefix_child
        && (options.registration_channel || has_launch_nonce)
}

pub(super) async fn register_connection<S, H>(
    stream: S,
    mut manifest: ModuleManifest,
    handler: H,
    options: ServeOptions,
) -> Result<Option<(ModuleHandle, ModuleServeFuture)>, SubcModuleError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: ModuleHandler,
{
    let control_ops = control_ops::validate(handler.control_ops())?;
    let (mut read_half, write_half) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel::<Frame>(EGRESS_BUFFER);
    let connection_runtime = tokio::runtime::Handle::current();
    let mut writer = tokio::spawn(drain_writer(write_half, rx));
    let handler = Arc::new(handler);
    let declaration = if options.prefix_child {
        None
    } else {
        handler.config_declaration()
    };

    let mut hello_ack = None;
    let mut can_resume = false;
    let handshake = async {
        let nonce = initial_launch_nonce()?;
        can_resume = advertises_upgrade(&options, nonce.is_some())
            && (nonce.is_none() || registration::is_launched_module(&manifest.module_id)?);
        stamp_launch_nonce_source(&mut manifest, nonce.as_deref());
        let frame = hello_frame(
            manifest.clone(),
            declaration.is_some(),
            &control_ops,
            nonce,
            can_resume,
            None,
        )?;
        send_outbound(&tx, frame).await?;
        // A validated ACK means the daemon accepted the module, even while the
        // module's callback is pending. Keep that boundary when the writer wins.
        hello_ack = Some(expect_hello_ack(&mut read_half).await?);
        handler
            .on_hello_ack(hello_ack.as_ref().expect("validated HELLO_ACK"))
            .await;
        Ok::<_, SubcModuleError>(())
    };
    let mut writer_result = None;
    let handshake_result = tokio::select! {
        biased;
        () = options.stop_handle.token.cancelled() => None,
        result = handshake => Some(result),
        result = &mut writer => {
            writer_result = Some(result);
            Some(Ok(()))
        }
    };
    match handshake_result {
        Some(Ok(())) => {}
        other => {
            let result = match other {
                None => {
                    let deadline = Instant::now() + options.writer_drain_timeout;
                    let result = timeout_at(deadline, send_goodbye(&tx)).await;
                    drop(tx);
                    let flush = finish_writer_and_drain(writer, &mut read_half, deadline).await;
                    handler.on_connection_end(ConnectionEnd::Closed).await;
                    match result.unwrap_or(Ok(())) {
                        Ok(()) | Err(SubcModuleError::WriterClosed) => {}
                        Err(error) => return Err(error),
                    }
                    match flush.0 {
                        Err(SubcModuleError::FrameIo(FrameIoError::Io(error)))
                            if is_disconnect(&error) => {}
                        result => result?,
                    }
                    return Ok(None);
                }
                Some(Err(error)) => error,
                Some(Ok(_)) => unreachable!(),
            };
            drop(tx);
            let deadline = Instant::now() + options.writer_drain_timeout;
            let _ = finish_writer_and_drain(writer, &mut read_half, deadline).await;
            return Err(result);
        }
    }
    let mut ack = match hello_ack {
        Some(ack) => ack,
        None => {
            return Err(writer_ended(
                writer_result.expect("writer ended before HELLO_ACK"),
            ))
        }
    };
    if options.prefix_child {
        // A prefix registration is brokered, not another primary channel. Keep
        // its registration RPCs while refusing owner-only operations locally.
        ack.subc_ops.retain(|op| {
            matches!(
                op.as_str(),
                "catalog.update" | "scope.describe" | "supervisor.live_roots" | "activity.record"
            )
        });
    }
    // The callback can be cancelled by a writer failure, but a failure after
    // acceptance belongs to the serving lifecycle, including its teardown hook.
    let writer_end = writer_result.map(writer_connection_end);

    let Some(connection_token) = checked_increment(&NEXT_MODULE_CONNECTION_TOKEN) else {
        drop(tx);
        let deadline = Instant::now() + options.writer_drain_timeout;
        if writer_end.is_none() {
            let _ = finish_writer_and_drain(writer, &mut read_half, deadline).await;
        }
        return Err(SubcModuleError::ConnectionTokenExhausted);
    };
    let close_token = CancellationToken::new();
    let handle = ModuleHandle::new(
        &ack,
        tx.clone(),
        connection_token,
        close_token,
        connection_runtime,
    );
    let shared = handle.shared();
    {
        let mut state = shared.lock_inner();
        state.current_provides = Some(manifest.provides.clone());
        if can_resume {
            state.upgrade.manifest = Some(manifest);
            state.upgrade.resume_token = ack.resume_token.take();
        }
    }
    shared
        .registration
        .prefix_child
        .store(options.prefix_child, Ordering::Release);
    let _ = shared.served_control_ops.set(control_ops);
    if options.registration_channel {
        registration::install_owner(&handle)?;
    }
    if let Some(declaration) = declaration {
        let _ = shared.config.declaration.set(declaration);
    }
    let serve_future = connection_serve_future_with_options(
        read_half,
        tx,
        handler,
        handle.clone(),
        writer,
        options,
        writer_end,
    );
    Ok(Some((handle, serve_future)))
}

pub(super) fn registration_channel_exit_code(
    result: &Result<ConnectionEnd, SubcModuleError>,
) -> i32 {
    if matches!(result, Ok(ConnectionEnd::Goodbye)) {
        0
    } else {
        1
    }
}

pub(super) fn connection_serve_future_with_options<R, H>(
    read_half: R,
    tx: mpsc::Sender<Frame>,
    handler: Arc<H>,
    handle: ModuleHandle,
    writer: tokio::task::JoinHandle<Result<(), WriterFailure>>,
    options: ServeOptions,
    writer_end: Option<Result<ConnectionEnd, SubcModuleError>>,
) -> ModuleServeFuture
where
    R: AsyncRead + Unpin + Send + 'static,
    H: ModuleHandler,
{
    // Before the caller gets the future (and before the serve loop reads its
    // first frame after HELLO_ACK), so the
    // handle's pool figures describe the pools the dispatcher will use.
    handle.install_dispatch_pools(handler.request_pools());
    Box::pin(serve_generations(
        Wire {
            reader: Box::new(read_half),
            tx,
            writer,
            writer_end,
        },
        handler,
        handle,
        options,
    ))
}

struct Wire {
    reader: Box<dyn AsyncRead + Unpin + Send>,
    tx: mpsc::Sender<Frame>,
    writer: tokio::task::JoinHandle<Result<(), WriterFailure>>,
    writer_end: Option<Result<ConnectionEnd, SubcModuleError>>,
}

struct Resumed {
    ack: ModuleHelloAckBody,
    sync: Option<Result<(CatalogUpdateGuard, CatalogUpdateWaiter), ScopeCallError>>,
}

async fn serve_generations<H: ModuleHandler>(
    mut wire: Wire,
    handler: Arc<H>,
    serve_handle: ModuleHandle,
    options: ServeOptions,
) -> Result<(), SubcModuleError> {
    let mut retired = Vec::new();
    let mut resumed: Option<Resumed> = None;
    loop {
        let Wire {
            reader: mut read_half,
            tx,
            mut writer,
            writer_end,
        } = wire;
        let shared = serve_handle.shared();
        let dispatcher = RequestDispatcher::with_pools(serve_handle.dispatch_pools());
        if resumed.is_none() && !options.prefix_child {
            config::start(serve_handle.clone(), Arc::clone(&handler), &dispatcher);
        }
        let mut writer_finished = writer_end.is_some();
        let mut loop_result = if let Some(end) = writer_end {
            end
        } else {
            let hooks = async {
                if let Some(Resumed { ack, sync }) = resumed.take() {
                    config::start(serve_handle.clone(), Arc::clone(&handler), &dispatcher);
                    handler.on_hello_ack(&ack).await;
                    let outcome = match sync {
                        Some(Ok((guard, reply))) => Some(
                            ScopeRequest::receive(guard, reply)
                                .await
                                .and_then(ScopeRequest::decode_sync),
                        ),
                        Some(Err(error)) => Some(Err(error)),
                        None => None,
                    };
                    handler.on_reconnected(&serve_handle, outcome).await;
                }
                std::future::pending().await
            };
            tokio::select! {
                biased;
                result = module_loop(&mut read_half, &tx, Arc::clone(&handler), serve_handle.clone(), &dispatcher, &options.stop_handle) => result,
                result = &mut writer => {
                    writer_finished = true;
                    writer_connection_end(result)
                }
                result = hooks => result,
            }
        };
        // A failed terminal enqueue can win the select just before the writer
        // reports why it closed. Classify that same disconnect before calling
        // the connection-end hook, rather than exposing a scheduling race.
        if matches!(loop_result, Err(SubcModuleError::WriterClosed)) && !writer_finished {
            writer_finished = true;
            loop_result = writer_connection_end((&mut writer).await);
        }
        shared.config.stop.cancel();
        if let Some((path, deadline)) = announced_upgrade(&options, &shared, &loop_result) {
            // The old connection cannot carry any more replies. Dispose of its
            // writer before resuming, including sends blocked on old capacity.
            serve_handle.close_connection();
            dispatcher.cut_for_upgrade();
            if !writer_finished {
                writer.abort();
                let _ = writer.await;
            }
            drop(tx);
            drop(read_half);
            let _ = timeout_at(deadline, dispatcher.tasks.wait()).await;
            retired.retain(|old: &RequestDispatcher| !old.data_tasks.is_empty());
            if !dispatcher.data_tasks.is_empty() {
                retired.push(dispatcher);
            }
            let session = match reconnect(path, deadline, &shared, &options.stop_handle).await {
                Some((stream, ack, manifest)) => install_resumed(
                    stream,
                    ack,
                    manifest,
                    &shared,
                    &serve_handle,
                    &*handler,
                    &options.stop_handle,
                )
                .await
                .map(Some),
                None => Ok(None),
            };
            if let Ok(Some((next_wire, next_hooks))) = session {
                (wire, resumed) = (next_wire, Some(next_hooks));
                continue;
            }
            let end = if options.stop_handle.token.is_cancelled() {
                ConnectionEnd::Closed
            } else {
                ConnectionEnd::Eof
            };
            finish_retired(&retired, Instant::now() + options.writer_drain_timeout).await;
            handler.on_connection_end(end).await;
            if options.registration_channel && end != ConnectionEnd::Closed {
                std::process::exit(registration_channel_exit_code(&Ok(end)));
            }
            return session.map(|_| ());
        }
        let deadline = Instant::now() + options.writer_drain_timeout;
        dispatcher.wait_for_draining_hooks_to_start().await;
        dispatcher.tasks.close();
        dispatcher.data_tasks.close();
        for old in &retired {
            old.cancel_all();
        }
        if matches!(loop_result, Ok(ConnectionEnd::Closed))
            && options.stop_handle.token.is_cancelled()
        {
            // Keep routes live until accepted work has sent its terminal reply.
            let _ = timeout_at(deadline, dispatcher.wait_tasks()).await;
        }
        let orderly = loop_result.is_ok();
        if let Some(error) = dispatcher.take_failure() {
            if !orderly || !matches!(error, SubcModuleError::WriterClosed) {
                loop_result = Err(error);
            }
        }
        dispatcher.cancel_all();
        let _ = timeout_at(deadline, dispatcher.wait_tasks()).await;
        let abandoned_replies = dispatcher
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        if let Some(error) = dispatcher.take_failure() {
            if !orderly || !matches!(error, SubcModuleError::WriterClosed) {
                loop_result = Err(error);
            }
        }
        dispatcher.abort_all();
        dispatcher.wait_tasks().await;
        finish_retired(&retired, deadline).await;
        let daemon_eof = matches!(loop_result, Ok(ConnectionEnd::Eof));
        let locally_closed = matches!(loop_result, Ok(ConnectionEnd::Closed));
        // The module ended the connection itself: a stop, an internal close
        // or a module-side error. Close it in order, so the daemon reads a
        // FIN rather than a reset. When the daemon ended it (EOF, GOODBYE or
        // a reset) or the writer already failed, there is nothing to drain.
        let module_ended =
            !writer_finished && matches!(loop_result, Ok(ConnectionEnd::Closed) | Err(_));
        let deadline = Instant::now() + options.writer_drain_timeout;
        if matches!(loop_result, Ok(ConnectionEnd::Closed))
            && options.stop_handle.token.is_cancelled()
        {
            match timeout_at(deadline, send_goodbye(&tx)).await {
                Ok(Ok(())) | Err(_) => {}
                Ok(Err(SubcModuleError::WriterClosed)) => {}
                Ok(Err(error)) => loop_result = Err(error),
            }
        }
        serve_handle.close_connection();
        drop(tx);
        let loop_result = match loop_result {
            Ok(end) => {
                handler.on_connection_end(end).await;
                Ok(end)
            }
            Err(error) => Err(error),
        };

        let (writer_result, undelivered_replies) = if writer_finished {
            (Ok(()), 0)
        } else if module_ended {
            finish_writer_and_drain(writer, &mut read_half, deadline).await
        } else {
            finish_writer(writer, deadline).await
        };
        let undelivered_replies = undelivered_replies as u64
            + dispatcher.undelivered_replies.load(Ordering::Relaxed)
            + abandoned_replies as u64;
        if daemon_eof && undelivered_replies > 0 {
            tracing::warn!(
                undelivered_replies,
                "daemon EOF: replies could not be delivered during drain"
            );
        }
        if options.registration_channel && !locally_closed {
            // An inherited registration cannot be recreated. Finish the hook
            // and teardown before letting the supervisor restart the process.
            // The supervisor treats an unrequested clean exit as a stop, so
            // only a daemon GOODBYE may exit successfully.
            std::process::exit(registration_channel_exit_code(&loop_result));
        }
        return match (loop_result.map(|_| ()), writer_result) {
            (Err(SubcModuleError::WriterClosed), Err(writer_error)) => Err(writer_error),
            (Err(loop_err), _) => Err(loop_err),
            (Ok(()), Ok(())) => Ok(()),
            // EOF or GOODBYE can precede a failed flush, especially when a
            // daemon restart outlasts its drain budget. Once the daemon has
            // ended the connection, Windows RST and Unix BrokenPipe are
            // consequences of that orderly end, not independent failures.
            (Ok(()), Err(SubcModuleError::FrameIo(FrameIoError::Io(err))))
                if is_disconnect(&err) =>
            {
                Ok(())
            }
            (Ok(()), Err(error)) => Err(error),
        };
    }
}

impl ModuleHandleShared {
    /// RPCs with a refused result (including catalog, scope and claims) use
    /// `module_reloading` to report that nothing was sent. Route pushes keep
    /// `WriterClosed` because the old destination is going away.
    /// [`ModuleHandle::confirm_operator`] keeps `PresenceUnavailable` so callers
    /// refuse the write when no confirmation can be sent.
    pub(super) fn check_upgrade_fence(&self) -> Result<(), CatalogUpdateError> {
        if self.lock_inner().upgrade.fenced() {
            Err(CatalogUpdateError::Rejected(upgrade_refusal()))
        } else {
            Ok(())
        }
    }

    async fn reserve_request<'a>(
        &self,
        writer: &'a mpsc::Sender<Frame>,
    ) -> Result<mpsc::Permit<'a, Frame>, CatalogUpdateError> {
        tokio::select! {
            biased;
            () = self.upgrade_notice.cancelled() => Err(CatalogUpdateError::Rejected(upgrade_refusal())),
            permit = writer.reserve() => permit.map_err(|_| CatalogUpdateError::ConnectionClosed),
        }
    }

    pub(super) async fn queue_request(
        &self,
        writer: &mpsc::Sender<Frame>,
        frame: Frame,
    ) -> Result<(), CatalogUpdateError> {
        let permit = self.reserve_request(writer).await?;
        let inner = self.lock_inner();
        if inner.upgrade.fenced() {
            return Err(CatalogUpdateError::Rejected(upgrade_refusal()));
        }
        if inner.closed {
            return Err(CatalogUpdateError::ConnectionClosed);
        }
        permit.send(frame);
        Ok(())
    }

    pub(super) async fn queue_catalog_change(
        &self,
        writer: &mpsc::Sender<Frame>,
        frame: Frame,
        update: CatalogManifestChange,
    ) -> Result<(), CatalogUpdateError> {
        let permit = self.reserve_request(writer).await?;
        let mut inner = self.lock_inner();
        if inner.upgrade.fenced() {
            return Err(CatalogUpdateError::Rejected(upgrade_refusal()));
        }
        if inner.closed {
            return Err(CatalogUpdateError::ConnectionClosed);
        }
        inner.catalog_changes.push_back(CatalogChange {
            corr: frame.header.corr,
            update,
            outcome: None,
        });
        permit.send(frame);
        Ok(())
    }
}

pub(super) struct ScopeRequest {
    guard: CatalogUpdateGuard,
    writer: mpsc::Sender<Frame>,
    frame: Frame,
    reply: CatalogUpdateWaiter,
}

impl ScopeRequest {
    fn prepare(
        shared: &Arc<ModuleHandleShared>,
        op: &'static str,
        request: &ModuleControlRequestFromModule,
        resume_sync: bool,
    ) -> Result<Self, ScopeCallError> {
        let body = serde_json::to_vec(request).map_err(|err| {
            ScopeCallError::Protocol(format!("failed to encode {op} request body: {err}"))
        })?;
        let (corr, writer, reply) = shared
            .begin_control_call(resume_sync)
            .map_err(ScopeCallError::from_control_error)?;
        let guard = CatalogUpdateGuard {
            shared: shared.clone(),
            corr,
        };
        let frame = Frame::build_with_version(
            shared.negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|err| {
            ScopeCallError::Protocol(format!("failed to build {op} request frame: {err}"))
        })?;
        Ok(Self {
            guard,
            writer,
            frame,
            reply,
        })
    }

    pub(super) async fn call(
        shared: &Arc<ModuleHandleShared>,
        op: &'static str,
        request: &ModuleControlRequestFromModule,
    ) -> Result<ModuleControlReply, ScopeCallError> {
        let Self {
            guard,
            writer,
            frame,
            reply,
        } = Self::prepare(shared, op, request, false)?;
        shared
            .queue_request(&writer, frame)
            .await
            .map_err(ScopeCallError::from_control_error)?;
        Self::receive(guard, reply).await
    }

    async fn receive(
        _guard: CatalogUpdateGuard,
        reply: CatalogUpdateWaiter,
    ) -> Result<ModuleControlReply, ScopeCallError> {
        match timeout(CATALOG_UPDATE_TIMEOUT, reply).await {
            Ok(Ok(reply)) => reply.map_err(ScopeCallError::from_control_error),
            Ok(Err(_)) => Err(ScopeCallError::ConnectionClosed),
            Err(_) => Err(ScopeCallError::Timeout),
        }
    }

    pub(super) fn check_fence(shared: &ModuleHandleShared) -> Result<(), ScopeCallError> {
        shared
            .check_upgrade_fence()
            .map_err(ScopeCallError::from_control_error)
    }

    pub(super) fn decode_sync(reply: ModuleControlReply) -> Result<ScopeSyncReply, ScopeCallError> {
        match reply
            .control()
            .map_err(ScopeCallError::from_control_error)?
        {
            ModuleControlResponseToModule::ScopeSync {
                generation,
                results,
                ended,
            } => Ok(ScopeSyncReply {
                generation,
                results,
                ended,
            }),
            other => Err(ScopeCallError::Protocol(format!(
                "unexpected {SCOPE_SYNC_OP} response: {other:?}"
            ))),
        }
    }
}

fn announced_upgrade<'a>(
    options: &'a ServeOptions,
    connection: &ModuleHandleShared,
    end: &Result<ConnectionEnd, SubcModuleError>,
) -> Option<(&'a Path, Instant)> {
    if !matches!(
        end,
        Ok(ConnectionEnd::Goodbye
            | ConnectionEnd::Eof
            | ConnectionEnd::Reset
            | ConnectionEnd::Closed)
    ) || options.stop_handle.token.is_cancelled()
    {
        return None;
    }
    Some((
        options.connection_file.as_deref()?,
        connection.lock_inner().upgrade.deadline?,
    ))
}

async fn finish_retired(retired: &[RequestDispatcher], deadline: Instant) {
    for old in retired {
        old.cancel_all();
    }
    for old in retired {
        let _ = timeout_at(deadline, old.data_tasks.wait()).await;
        old.abort_all();
        old.data_tasks.wait().await;
    }
}

fn initial_launch_nonce() -> Result<Option<String>, SubcModuleError> {
    if registration::supervised() {
        Ok(None)
    } else if let Some(retained) = retained_launch_nonce() {
        Ok(Some(retained))
    } else {
        launch_nonce()
            .map_err(SubcModuleError::LaunchNonce)
            .map(|nonce| nonce.map(|nonce| nonce.value().to_string()))
    }
}

fn hello_frame(
    manifest: ModuleManifest,
    config_gate: bool,
    extra_control_ops: &[String],
    launch_nonce: Option<String>,
    live_upgrade: bool,
    resume_token: Option<String>,
) -> Result<Frame, SubcModuleError> {
    let mut control_ops = vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()];
    if config_gate {
        control_ops.push(config::CONFIG_REVIEW_OP.to_owned());
    }
    control_ops.extend_from_slice(extra_control_ops);
    let body = serde_json::to_vec(&ModuleHelloBody {
        module_capabilities: live_upgrade.then(|| vec![LIVE_UPGRADE_CAPABILITY.to_owned()]),
        resume_token,
        manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: Some(control_ops),
        launch_nonce,
    })
    .map_err(SubcModuleError::Json)?;
    Frame::build(FrameType::Hello, control_flags(), 0, 0, HELLO_CORR, body)
        .map_err(SubcModuleError::FrameBuild)
}

async fn reconnect(
    path: &Path,
    deadline: Instant,
    old: &ModuleHandleShared,
    stop: &ModuleStopHandle,
) -> Option<(Box<dyn ModuleStream>, ModuleHelloAckBody, ModuleManifest)> {
    let (manifest, token) = {
        let old = old.lock_inner();
        (
            old.upgrade.manifest.clone()?,
            old.upgrade.resume_token.clone()?,
        )
    };
    let hello = hello_frame(
        manifest.clone(),
        old.config.declaration.get().is_some(),
        old.served_control_ops
            .get()
            .map(Vec::as_slice)
            .unwrap_or_default(),
        None,
        true,
        Some(token),
    )
    .ok()?;
    let attempts = async {
        loop {
            let next = Instant::now() + Duration::from_millis(100);
            match resume_attempt(path, &hello).await {
                Ok((stream, ack)) => return Some((stream, ack, manifest)),
                Err(error) if retry_resume(&error) => tokio::time::sleep_until(next).await,
                // Do not log daemon-provided handshake bodies, which might
                // include the resume credential.
                Err(_) => return None,
            }
        }
    };
    tokio::select! {
        biased;
        () = stop.token.cancelled() => None,
        () = tokio::time::sleep_until(deadline) => None,
        result = attempts => result,
    }
}

fn retry_resume(error: &SubcModuleError) -> bool {
    match error {
        SubcModuleError::HelloRejected { body } => {
            error_codes::is_retryable_resume_hello(&body.code)
        }
        SubcModuleError::ConnectionFile {
            source: ConnectionFileError::Io { source, .. },
            ..
        }
        | SubcModuleError::Connect { source, .. } => matches!(
            source.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ),
        SubcModuleError::NoEndpoint { .. }
        | SubcModuleError::Auth { .. }
        | SubcModuleError::FrameIo(FrameIoError::Io(_))
        | SubcModuleError::ConnectionClosedBeforeHelloAck => true,
        _ => false,
    }
}

async fn resume_attempt(
    path: &Path,
    hello: &Frame,
) -> Result<(Box<dyn ModuleStream>, ModuleHelloAckBody), SubcModuleError> {
    let mut stream = connect_resume_local(path).await?;
    write_frame(&mut stream, hello)
        .await
        .map_err(SubcModuleError::FrameIo)?;
    let ack = expect_hello_ack(&mut stream).await?;
    Ok((stream, ack))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn connect_resume_local(path: &Path) -> Result<Box<dyn ModuleStream>, SubcModuleError> {
    use connection_file::LocalEndpoint;
    let read = connection_file::read_for_client_with_local_endpoint(path).map_err(|source| {
        SubcModuleError::ConnectionFile {
            path: path.to_owned(),
            source,
        }
    })?;
    let Some(LocalEndpoint::Unix { path: socket }) = read.local_endpoint else {
        return Err(SubcModuleError::NoEndpoint {
            path: path.to_owned(),
        });
    };
    let mut stream = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|source| SubcModuleError::Connect {
            path: path.to_owned(),
            endpoint: socket.display().to_string(),
            source,
        })?;
    authenticate_client(&mut stream, &read.info, AUTH_DEADLINE)
        .await
        .map_err(|source| SubcModuleError::Auth {
            path: path.to_owned(),
            endpoint: socket.display().to_string(),
            source,
        })?;
    Ok(Box::new(stream))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn connect_resume_local(path: &Path) -> Result<Box<dyn ModuleStream>, SubcModuleError> {
    Err(SubcModuleError::NoEndpoint {
        path: path.to_owned(),
    })
}

async fn install_resumed<H: ModuleHandler>(
    stream: Box<dyn ModuleStream>,
    ack: ModuleHelloAckBody,
    manifest: ModuleManifest,
    old: &ModuleHandleShared,
    handle: &ModuleHandle,
    handler: &H,
    stop: &ModuleStopHandle,
) -> Result<(Wire, Resumed), SubcModuleError> {
    let token = checked_increment(&NEXT_MODULE_CONNECTION_TOKEN)
        .ok_or(SubcModuleError::ConnectionTokenExhausted)?;
    let (reader, writer) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel(EGRESS_BUFFER);
    let connection = ModuleHandleShared::new(
        &ack,
        tx.clone(),
        token,
        CancellationToken::new(),
        old.connection_runtime.clone(),
    );
    if let Some(declaration) = old.config.declaration.get() {
        let _ = connection.config.declaration.set(declaration.clone());
    }
    if let Some(control_ops) = old.served_control_ops.get() {
        let _ = connection.served_control_ops.set(control_ops.clone());
    }
    if !old.registration.is_active() {
        registration::install_resumed_nonce_owner(handle)?;
    }
    registration::rebind_owner(&connection);
    *connection
        .dispatch_pools
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Arc::new(DispatchPools::new(handler.request_pools()));
    {
        let mut state = connection.lock_inner();
        state.current_provides = Some(manifest.provides.clone());
        state.upgrade = UpgradeState {
            manifest: Some(manifest),
            resume_token: ack.resume_token.clone(),
            starting: true,
            deadline: None,
        };
    }
    handle.generations.install(&ack, connection);
    let shared = handle.shared();
    let request = tokio::select! {
        biased;
        () = stop.token.cancelled() => None,
        request = handler.resume_scope_sync(handle) => request,
    };
    let sync = request.map(|ResumeScopeSync { generation, scopes }| {
        let request = ModuleControlRequestFromModule::ScopeSync { generation, scopes };
        let ScopeRequest {
            guard,
            writer,
            frame,
            reply,
        } = ScopeRequest::prepare(&shared, SCOPE_SYNC_OP, &request, true)?;
        // This channel is still empty and all other sends are fenced. Queue the
        // sync before starting the reader or releasing any other sender.
        writer
            .try_send(frame)
            .map_err(|_| ScopeCallError::ConnectionClosed)?;
        Ok((guard, reply))
    });
    let writer = tokio::spawn(drain_writer(writer, rx));
    shared.lock_inner().upgrade.starting = false;
    Ok((
        Wire {
            reader: Box::new(reader),
            tx,
            writer,
            writer_end: None,
        },
        Resumed { ack, sync },
    ))
}

impl RequestDispatcher {
    pub(super) fn spawn_data(&self, future: impl Future<Output = ()> + Send + 'static) {
        let task = self.data_tasks.spawn(future);
        let mut aborts = self.data_aborts.lock().unwrap_or_else(|e| e.into_inner());
        aborts.retain(|task| !task.is_finished());
        aborts.push(task.abort_handle());
    }

    fn cut_for_upgrade(&self) {
        self.tasks.close();
        self.data_tasks.close();
        self.pools.close();
        let in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        for ((channel, _, _), token) in in_flight.iter() {
            if *channel == 0 {
                token.cancel();
            }
        }
        abort_tasks(&self.aborts);
    }

    async fn wait_tasks(&self) {
        tokio::join!(self.tasks.wait(), self.data_tasks.wait());
    }
}

pub(super) fn abort_tasks(tasks: &Mutex<Vec<tokio::task::AbortHandle>>) {
    for task in tasks.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
        task.abort();
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
