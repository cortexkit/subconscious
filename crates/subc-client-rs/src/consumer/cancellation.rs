use super::*;

/// Explicit cancellation for one unary request. Dropping this handle sends nothing.
///
/// The reply future remains live after cancellation, including its original deadline.
/// A successful Response is returned with its body intact, even after Cancel was sent.
pub struct CancelHandle {
    state: Arc<UnaryCancel>,
}

/// The local outcome of an explicit cancellation, not a provider acknowledgement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelOutcome {
    /// One Cancel was queued on the connection. Delivery is not acknowledged.
    Sent,
    /// Cancellation was remembered while the lazy future prepares the request.
    /// It will be sent after the Request is queued, if the future is still driven.
    Requested,
    /// A Cancel has already been queued for this call.
    AlreadyCancelled,
    /// The request settled, timed out, or its future was dropped.
    Settled,
    /// The request's route is no longer installed on its original connection.
    RouteClosed,
    /// The original connection ended or was replaced.
    ConnectionDropped,
    /// Writer capacity did not become available before the request's deadline.
    DeadlineElapsed,
}

impl CancelHandle {
    /// Queue at most one header-only Cancel, without consuming the reply.
    ///
    /// Waits only for writer capacity, bounded by the request's deadline, never for
    /// the provider's reply. Repeated calls and calls after settlement are no-ops.
    /// Before the Request is queued, records the intent for the reply future to
    /// send it. Keep polling that future to receive the provider's terminal frame.
    /// Dropping the future instead removes its registration and sends no Cancel.
    pub async fn cancel(&self) -> CancelOutcome {
        self.state.cancel().await
    }
}

#[derive(Clone, Copy)]
struct ActiveRequest {
    key: PendingKey,
    priority: Priority,
    deadline: Instant,
}

#[derive(Default)]
struct CancelState {
    active: Option<ActiveRequest>,
    requested: bool,
    sent: bool,
    finished: bool,
}

pub(super) struct UnaryCancel {
    shared: Arc<Shared>,
    state: Mutex<CancelState>,
}

impl UnaryCancel {
    fn lock(&self) -> MutexGuard<'_, CancelState> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub(super) fn requested(&self) -> bool {
        self.lock().requested
    }

    pub(super) fn activate(&self, key: PendingKey, priority: Priority, deadline: Instant) {
        self.lock().active = Some(ActiveRequest {
            key,
            priority,
            deadline,
        });
    }

    fn check(
        &self,
        inner: &Inner,
        state: &CancelState,
        active: ActiveRequest,
    ) -> Option<CancelOutcome> {
        if state.sent {
            return Some(CancelOutcome::AlreadyCancelled);
        }
        if inner.closed || inner.generation != active.key.generation || inner.writer.is_none() {
            return Some(CancelOutcome::ConnectionDropped);
        }
        let handle = RouteHandle::new(active.key.channel, active.key.epoch, active.key.generation);
        if !route_is_installed(inner, handle) {
            return Some(CancelOutcome::RouteClosed);
        }
        if state.finished || !inner.pending.contains_key(&active.key) {
            return Some(CancelOutcome::Settled);
        }
        if Instant::now() >= active.deadline {
            return Some(CancelOutcome::DeadlineElapsed);
        }
        None
    }

    pub(super) async fn cancel(&self) -> CancelOutcome {
        let active = {
            let mut state = self.lock();
            state.requested = true;
            match state.active {
                Some(active) => active,
                None if state.finished => return CancelOutcome::Settled,
                None => return CancelOutcome::Requested,
            }
        };
        let writer = {
            let inner = self.shared.lock_inner();
            if let Some(outcome) = self.check(&inner, &self.lock(), active) {
                return outcome;
            }
            inner.writer.as_ref().unwrap().clone()
        };
        let permit = match timeout_at(active.deadline, writer.reserve()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return CancelOutcome::ConnectionDropped,
            Err(_) => return CancelOutcome::DeadlineElapsed,
        };

        // Serialize the final liveness check and enqueue with terminal settlement.
        // Keep the pending entry: Cancel is advisory, not a local completion.
        let inner = self.shared.lock_inner();
        let mut state = self.lock();
        if let Some(outcome) = self.check(&inner, &state, active) {
            return outcome;
        }
        let frame = Frame::build(
            FrameType::Cancel,
            Flags::new(false, active.priority, false),
            active.key.channel,
            active.key.epoch,
            active.key.corr,
            Vec::new(),
        )
        .expect("a header-only Cancel on a registered unary route is valid");
        permit.send(WriteCommand {
            frame,
            pending: None,
        });
        state.sent = true;
        CancelOutcome::Sent
    }
}

/// Captured outside the async body so dropping even an unpolled future marks it done.
pub(super) struct CancelRegistration {
    pub(super) state: Arc<UnaryCancel>,
}

impl CancelRegistration {
    pub(super) fn new(shared: Arc<Shared>) -> (CancelHandle, Self) {
        let state = Arc::new(UnaryCancel {
            shared,
            state: Mutex::new(CancelState::default()),
        });
        (
            CancelHandle {
                state: Arc::clone(&state),
            },
            Self { state },
        )
    }
}

impl Drop for CancelRegistration {
    fn drop(&mut self) {
        self.state.lock().finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_cancel() -> (
        Arc<UnaryCancel>,
        mpsc::Receiver<WriteCommand>,
        oneshot::Receiver<PendingResult>,
    ) {
        let shared = Arc::new(Shared::new(
            PathBuf::from("unused"),
            ConsumerOptions::default(),
        ));
        let key = PendingKey {
            generation: 1,
            channel: 7,
            epoch: 9,
            corr: 42,
        };
        let (writer, reader) = mpsc::channel(1);
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = shared.lock_inner();
            inner.generation = key.generation;
            inner.writer = Some(writer);
            inner.route_epochs.insert(
                key.channel,
                RouteHandle::new(key.channel, key.epoch, key.generation),
            );
            inner
                .pending
                .insert(key, PendingEntry::unary(tx, false, None, None));
        }
        let state = Arc::new(UnaryCancel {
            shared,
            state: Mutex::new(CancelState::default()),
        });
        state.activate(
            key,
            Priority::Interactive,
            Instant::now() + Duration::from_secs(1),
        );
        (state, reader, rx)
    }

    fn fill_writer(cancel: &UnaryCancel) {
        cancel
            .shared
            .lock_inner()
            .writer
            .as_ref()
            .unwrap()
            .try_send(WriteCommand {
                frame: Frame::build(
                    FrameType::Request,
                    Flags::new(false, Priority::Passive, false),
                    0,
                    0,
                    1,
                    vec![],
                )
                .unwrap(),
                pending: None,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_rechecks_settlement_and_teardown_after_writer_capacity_wait() {
        for expected in [
            CancelOutcome::Settled,
            CancelOutcome::RouteClosed,
            CancelOutcome::ConnectionDropped,
        ] {
            let (cancel, mut writer, _reply) = active_cancel();
            fill_writer(&cancel);
            let cancelling = cancel.cancel();
            tokio::pin!(cancelling);
            tokio::select! {
                biased;
                outcome = &mut cancelling => panic!("full writer must wait, got {outcome:?}"),
                () = tokio::task::yield_now() => {}
            }
            let key = cancel.lock().active.unwrap().key;
            {
                let mut inner = cancel.shared.lock_inner();
                match expected {
                    CancelOutcome::Settled => {
                        inner.pending.remove(&key);
                    }
                    CancelOutcome::RouteClosed => {
                        inner.route_epochs.remove(&key.channel);
                    }
                    CancelOutcome::ConnectionDropped => {
                        inner.generation += 1;
                    }
                    _ => unreachable!(),
                }
            }
            let _original = writer.recv().await.unwrap();
            assert_eq!(cancelling.await, expected);
            assert!(
                writer.try_recv().is_err(),
                "no late Cancel after settlement or teardown"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_full_writer_is_bounded_by_request_deadline() {
        let (cancel, mut writer, _reply) = active_cancel();
        fill_writer(&cancel);
        assert_eq!(cancel.cancel().await, CancelOutcome::DeadlineElapsed);
        let _original = writer.recv().await.unwrap();
        assert!(writer.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancellation_after_ingress_settlement_does_not_require_reply_poll() {
        let (cancel, mut writer, reply) = active_cancel();
        let key = cancel.lock().active.unwrap().key;
        cancel.shared.settle_pending(
            key,
            PendingTerminal::Response {
                generation: key.generation,
                body: b"finished".to_vec(),
            },
        );
        assert_eq!(cancel.cancel().await, CancelOutcome::Settled);
        assert!(writer.try_recv().is_err());
        match reply.await.unwrap().into_call_result().unwrap() {
            TerminalFrame::Response { body, .. } => assert_eq!(body, b"finished"),
            _ => panic!("expected the intact Response"),
        }
    }
}
