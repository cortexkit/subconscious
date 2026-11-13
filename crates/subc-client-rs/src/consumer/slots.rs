use super::*;

/// Shared between a caller and its wire correlation. Dropping the caller does
/// not return credit while the correlation still owns a reference.
pub(super) struct RoutePermit {
    route: Option<RouteState>,
    permits: Mutex<Option<(OwnedSemaphorePermit, Option<OwnedSemaphorePermit>)>>,
}

impl RoutePermit {
    pub(super) fn legacy(permit: OwnedSemaphorePermit) -> Arc<Self> {
        Arc::new(Self {
            route: None,
            permits: Mutex::new(Some((permit, None))),
        })
    }

    pub(super) fn release(&self) {
        self.permits
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
    }

    pub(super) fn opted_in(&self) -> bool {
        self.route.as_ref().is_some_and(|route| route.opted_in)
    }

    pub(super) fn belongs_to(&self, route: &RouteState) -> bool {
        self.route
            .as_ref()
            .is_some_and(|held| held.handle == route.handle && Arc::ptr_eq(&held.sem, &route.sem))
    }
}

impl RouteState {
    pub(super) fn windows(
        handle: RouteHandle,
        request_window: Option<usize>,
        watch_window: Option<usize>,
    ) -> Self {
        Self {
            handle,
            sem: Arc::new(Semaphore::new(
                request_window.unwrap_or(DEFAULT_ROUTE_WINDOW),
            )),
            watch_sem: request_window
                .and(watch_window)
                .map(|n| Arc::new(Semaphore::new(n))),
            opted_in: request_window.is_some(),
        }
    }

    pub(super) fn close(&self) {
        self.sem.close();
        if let Some(watches) = &self.watch_sem {
            watches.close();
        }
    }

    pub(super) async fn acquire(
        &self,
        watch: bool,
        deadline: Instant,
        cancel: Option<&Arc<UnaryCancel>>,
    ) -> Result<Arc<RoutePermit>, CallError> {
        let acquire = async {
            // Waiting watches never occupy total capacity. Tokio semaphores
            // hand each lane to its oldest remaining waiter.
            let watches = if watch {
                match &self.watch_sem {
                    Some(sem) => Some(Arc::clone(sem).acquire_owned().await?),
                    None => None,
                }
            } else {
                None
            };
            let total = Arc::clone(&self.sem).acquire_owned().await?;
            Ok::<_, tokio::sync::AcquireError>((total, watches))
        };
        tokio::pin!(acquire);
        // Try capacity before a remembered cancellation. Once admission actually
        // waits, cancellation must still win a race with newly released capacity.
        let immediate = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(acquire.as_mut(), cx))
        })
        .await;
        let cancellation = async {
            match cancel.filter(|_| self.opted_in) {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let permits = match immediate {
            std::task::Poll::Ready(result) => {
                result.map_err(|_| CallError::StaleRouteHandle(self.handle))?
            }
            std::task::Poll::Pending => tokio::select! {
                biased;
                () = cancellation => return Err(CallError::not_sent("request cancelled waiting for route flow-control")),
                result = timeout_at(deadline, acquire) => match result {
                    Ok(Ok(permits)) => permits,
                    Ok(Err(_)) => return Err(CallError::StaleRouteHandle(self.handle)),
                    Err(_) => return Err(CallError::not_sent("call deadline elapsed waiting for route flow-control")),
                },
            },
        };
        Ok(Arc::new(RoutePermit {
            route: Some(self.clone()),
            permits: Mutex::new(Some(permits)),
        }))
    }
}

pub(super) fn busy_error(body: &ErrorBody) -> CallError {
    CallError::not_sent_with_cause(
        format!(
            "daemon refused before forwarding: {} ({})",
            body.code, body.message
        ),
        NotSentCause::DaemonRefusedBeforeForward {
            code: body.code.clone(),
        },
    )
}

pub(super) fn is_refusal_on_route(trusted: bool, flags: Flags, code: &str, opted_in: bool) -> bool {
    error_codes::is_route_busy_retryable(flags, code, opted_in)
        || is_daemon_refusal_before_forward(trusted, flags, code)
}

impl Shared {
    pub(super) async fn busy_backoff(
        &self,
        deadline: Instant,
        delay: Duration,
        cancel: Option<&Arc<UnaryCancel>>,
    ) -> bool {
        let cancellation = async {
            match cancel {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            () = self.close_token.cancelled() => false,
            () = cancellation => false,
            () = tokio::time::sleep_until((Instant::now() + self.jittered(delay)).min(deadline)) => Instant::now() < deadline,
        }
    }

    pub(super) async fn retry_subscription(
        self: Arc<Self>,
        key: PendingKey,
        mut retry: SubscriptionRetry,
        refusal: PendingTerminal,
    ) {
        tokio::select! {
            biased;
            () = retry.position.stop.cancelled() => return,
            () = self.close_token.cancelled() => return,
            () = tokio::time::sleep_until((Instant::now() + self.jittered(retry.delay)).min(retry.deadline)) => {},
        }
        if Instant::now() >= retry.deadline {
            if let Some(entry) = self.lock_inner().remove_pending(&key) {
                entry.settle_terminal(refusal);
            }
            return;
        }
        let handle = RouteHandle::new(key.channel, key.epoch, key.generation);
        let route = match self.route_state(handle) {
            Ok(route) if !route.sem.is_closed() => route,
            _ => {
                let (entry, reason) = {
                    let mut inner = self.lock_inner();
                    (inner.remove_pending(&key), inner.route_end(handle.channel))
                };
                if let Some(entry) = entry {
                    settle_route_pending_entries(
                        vec![entry],
                        "route closed during subscription retry".into(),
                        reason,
                    );
                }
                return;
            }
        };
        let (next, writer) = match self.allocate_request(Some(handle), handle.channel, handle.epoch)
        {
            Ok(allocated) => allocated,
            Err(error) => {
                if let Some(entry) = self.lock_inner().remove_pending(&key) {
                    entry.settle_call_error(error);
                }
                return;
            }
        };
        retry.delay = (retry.delay * 2).min(Duration::from_secs(1));
        retry.frame.header.corr = next.corr;
        let frame = retry.frame.clone();
        {
            let mut inner = self.lock_inner();
            let Some(mut entry) = inner.remove_pending(&key) else {
                return;
            };
            if retry.position.stop.is_cancelled() {
                entry.settle_subscription_result(Ok(()));
                return;
            }
            let PendingCompletion::Subscription { _permit, .. } = &entry.completion else {
                unreachable!()
            };
            if !_permit.belongs_to(&route) {
                drop(inner);
                entry.settle_call_error(self.route_not_sent_error(handle));
                return;
            }
            inner.wire_slots.insert(next, Arc::clone(_permit));
            *retry
                .position
                .key
                .lock()
                .unwrap_or_else(|err| err.into_inner()) = next;
            entry.retry = Some(retry.clone());
            entry.send_deadline = Some(retry.deadline);
            inner.pending.insert(next, entry);
        }
        let sent = timeout_at(
            retry.deadline,
            writer.send(WriteCommand {
                frame,
                pending: Some(next),
            }),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            let entry = self.lock_inner().remove_pending(&next);
            if let Some(entry) = entry {
                let accepted = entry.accepted;
                entry.settle_call_error(classify_failure(
                    accepted,
                    "writer closed during subscription retry",
                    OutcomeUnknownCause::WriterClosed,
                ));
            }
        }
    }
}
