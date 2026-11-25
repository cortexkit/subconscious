//! Bounded channel-zero streams on a module's registered connection.

use super::*;
use subc_control::ScopeWatchRequest;

pub(crate) struct ControlStreamSender {
    events: mpsc::Sender<Frame>,
    terminal: oneshot::Sender<Result<(), ScopeCallError>>,
}

struct ModuleControlSubscription {
    shared: Arc<ModuleHandleShared>,
    corr: u64,
    events: mpsc::Receiver<Frame>,
    terminal: Option<oneshot::Receiver<Result<(), ScopeCallError>>>,
    finished: bool,
}

impl ModuleHandle {
    /// Watch scope epochs on this registered connection. The initial snapshot
    /// closes the gap for endings before open; subsequent events are terminal
    /// for each selector. Drop or unsubscribe, then reopen to replace the set.
    /// A lost connection ends the feed; open a new watch after registering again.
    pub async fn scope_watch(
        &self,
        scopes: Vec<ScopeWatchSelector>,
    ) -> Result<ScopeSubscription, ScopeCallError> {
        let shared = self.shared();
        crate::live_upgrade::ScopeRequest::check_fence(&shared)?;
        if !shared.supports_scope_watch {
            return Err(ScopeCallError::NotSupported {
                op: subc_control::ops::SCOPE_WATCH,
            });
        }
        let body = serde_json::to_vec(&ScopeWatchRequest::new(scopes))
            .map_err(|err| ScopeCallError::Protocol(err.to_string()))?;
        // One snapshot and at most 256 terminal events can arrive before the
        // caller next polls; the reader must never block module RPC dispatch.
        let mut inner = Self::control_subscribe(shared, body, 257).await?;
        let first = timeout(CATALOG_UPDATE_TIMEOUT, inner.next())
            .await
            .map_err(|_| ScopeCallError::Timeout)??
            .ok_or_else(|| {
                ScopeCallError::Protocol("scope.watch ended before its snapshot".into())
            })?;
        if first.header.ty != FrameType::Response {
            return Err(ScopeCallError::Protocol(
                "scope.watch did not open with a snapshot RESPONSE".into(),
            ));
        }
        let initial = serde_json::from_slice(&first.body).map_err(|err| {
            ScopeCallError::Protocol(format!("invalid scope.watch snapshot: {err}"))
        })?;
        Ok(ScopeSubscription { initial, inner })
    }

    async fn control_subscribe(
        shared: Arc<ModuleHandleShared>,
        body: Vec<u8>,
        capacity: usize,
    ) -> Result<ModuleControlSubscription, ScopeCallError> {
        let (events, rx) = mpsc::channel(capacity);
        let (terminal, closed) = oneshot::channel();
        let (corr, writer) = {
            let mut state = shared.lock_inner();
            if state.closed {
                return Err(ScopeCallError::ConnectionClosed);
            }
            let writer = state
                .writer
                .clone()
                .ok_or(ScopeCallError::ConnectionClosed)?;
            let corr =
                next_module_control_corr(&mut state).ok_or(ScopeCallError::ConnectionClosed)?;
            state
                .pending_control_streams
                .insert(corr, ControlStreamSender { events, terminal });
            (corr, writer)
        };
        let stream = ModuleControlSubscription {
            shared: Arc::clone(&shared),
            corr,
            events: rx,
            terminal: Some(closed),
            finished: false,
        };
        let frame = Frame::build_with_version(
            shared.negotiated_ver,
            FrameType::Request,
            Flags(control_flags().0 | subc_protocol::FLAG_SUBSCRIPTION),
            0,
            0,
            corr,
            body,
        )
        .map_err(|err| ScopeCallError::Protocol(err.to_string()))?;
        timeout(CATALOG_UPDATE_TIMEOUT, shared.queue_request(&writer, frame))
            .await
            .map_err(|_| ScopeCallError::Timeout)?
            .map_err(ScopeCallError::from_control_error)?;
        Ok(stream)
    }

    pub(crate) fn handle_control_stream_frame(&self, frame: Frame) -> Option<Frame> {
        let shared = self.shared();
        let mut state = shared.lock_inner();
        let corr = frame.header.corr;
        let Some(stream) = state.pending_control_streams.remove(&corr) else {
            return Some(frame);
        };
        let mut cancel = false;
        let terminal = match frame.header.ty {
            FrameType::Error => Err(match serde_json::from_slice::<ErrorBody>(&frame.body) {
                Ok(body) => ScopeCallError::Refused {
                    code: body.code,
                    message: body.message,
                },
                Err(err) => ScopeCallError::Protocol(err.to_string()),
            }),
            FrameType::StreamEnd => Ok(()),
            FrameType::Response | FrameType::StreamData => {
                let last = frame.header.flags.is_last();
                if stream.events.try_send(frame).is_err() {
                    cancel = true;
                    Err(ScopeCallError::Protocol(
                        "module control subscription event buffer exhausted".into(),
                    ))
                } else if last {
                    Ok(())
                } else {
                    state.pending_control_streams.insert(corr, stream);
                    return None;
                }
            }
            _ => {
                state.pending_control_streams.insert(corr, stream);
                return Some(frame);
            }
        };
        let _ = stream.terminal.send(terminal);
        let writer = cancel.then(|| state.writer.clone()).flatten();
        drop(state);
        if let Some(writer) = writer {
            send_cancel(&shared, corr, writer);
        }
        None
    }
}

impl ModuleControlSubscription {
    async fn next(&mut self) -> Result<Option<Frame>, ScopeCallError> {
        if self.finished {
            return Ok(None);
        }
        if let Some(frame) = self.events.recv().await {
            return Ok(Some(frame));
        }
        self.finished = true;
        match self.terminal.take() {
            Some(terminal) => terminal
                .await
                .unwrap_or(Err(ScopeCallError::ConnectionClosed))
                .map(|()| None),
            None => Ok(None),
        }
    }

    fn cancel(&mut self) {
        self.finished = true;
        let writer = {
            let mut state = self.shared.lock_inner();
            if state.pending_control_streams.remove(&self.corr).is_none() {
                return;
            }
            state.writer.clone()
        };
        if let Some(writer) = writer {
            send_cancel(&self.shared, self.corr, writer);
        }
    }
}

fn send_cancel(shared: &ModuleHandleShared, corr: u64, writer: mpsc::Sender<Frame>) {
    if let Ok(frame) = Frame::build_with_version(
        shared.negotiated_ver,
        FrameType::Cancel,
        control_flags(),
        0,
        0,
        corr,
        vec![],
    ) {
        shared.connection_runtime.spawn(async move {
            let _ = timeout(CATALOG_UPDATE_TIMEOUT, writer.send(frame)).await;
        });
    }
}

impl Drop for ModuleControlSubscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// A scope lifecycle feed on the module's registered connection. Initial
/// states are available before any push; queued pushes precede terminal errors.
pub struct ScopeSubscription {
    pub initial: ScopeWatchResponse,
    inner: ModuleControlSubscription,
}

impl ScopeSubscription {
    pub async fn next(&mut self) -> Result<Option<ScopeWatchPush>, ScopeCallError> {
        let Some(frame) = self.inner.next().await? else {
            return Ok(None);
        };
        serde_json::from_slice(&frame.body)
            .map(Some)
            .map_err(|err| {
                self.inner.cancel();
                ScopeCallError::Protocol(format!("invalid scope.watch event: {err}"))
            })
    }

    pub fn unsubscribe(&mut self) {
        self.inner.cancel();
    }
}
