use super::*;
use subc_control::{CatalogWatchRequest, CatalogWatchResponse};

impl SubcConsumer {
    /// Target authority captured on this live route's open reply. An older
    /// daemon, an incomplete target, or a retired handle provides no target authority.
    /// Catalog entries are discovery facts, not a substitute for this value.
    pub fn route_target(&self, handle: &RouteHandle) -> Option<RouteOpenTarget> {
        let inner = self.shared.lock_inner();
        if inner.writer.is_none()
            || inner.route_epochs.get(&handle.channel) != Some(handle)
            || inner.retired_routes.contains(handle)
        {
            return None;
        }
        inner.route_targets.get(handle).cloned()
    }

    /// Follow catalog generations on channel 0. The first response carries the
    /// current generation; later advances may coalesce. Reconnect ends the feed:
    /// open a new watch and refresh the catalog rather than assuming no changes.
    pub async fn catalog_watch(&self) -> Result<CatalogSubscription, CallError> {
        let deadline = Instant::now() + self.shared.opts.call_timeout;
        let body = serde_json::to_vec(&CatalogWatchRequest::new())
            .map_err(|err| CallError::not_sent(err.to_string()))?;
        loop {
            self.shared.ensure_connected_for_call(deadline).await?;
            let permit = Arc::new(Semaphore::new(1))
                .acquire_owned()
                .await
                .map_err(|_| CallError::not_sent("catalog subscription permit closed"))?;
            match self
                .shared
                .send_subscription(SubscriptionSend {
                    expected_handle: None,
                    channel: 0,
                    epoch: 0,
                    body: body.clone(),
                    priority: Priority::Interactive,
                    admission_class: AdmissionClass::Normal,
                    event_buffer: DEFAULT_SUBSCRIPTION_EVENT_BUFFER,
                    deadline,
                    permit: RoutePermit::legacy(permit),
                })
                .await
            {
                Ok(inner) => {
                    return Ok(CatalogSubscription {
                        inner,
                        finished: false,
                    })
                }
                Err(err) if err.is_not_sent() && Instant::now() < deadline => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

/// A held catalog generation feed. Drop or unsubscribe to cancel it. Queued
/// generations are delivered before a terminal refusal or connection failure.
pub struct CatalogSubscription {
    inner: Subscription,
    finished: bool,
}

impl CatalogSubscription {
    /// Read the next generation, retaining daemon refusal codes in `CallError`.
    /// Once the feed ends, subsequent reads return `Ok(None)`.
    pub async fn next(&mut self) -> Result<Option<CatalogWatchResponse>, CallError> {
        if self.finished {
            return Ok(None);
        }
        if let Some(body) = self.inner.events().recv().await {
            return serde_json::from_slice(&body).map(Some).map_err(|err| {
                self.finished = true;
                let _ = self.inner.unsubscribe();
                CallError::invalid_response(format!("invalid catalog.watch response: {err}"))
            });
        }
        self.finished = true;
        self.inner.closed().await.map(|()| None)
    }

    pub fn unsubscribe(&self) -> Result<(), CallError> {
        self.inner.unsubscribe()
    }
}

pub(super) fn decode_route_open_response(
    body: &[u8],
) -> serde_json::Result<(ClientControlResponse, RouteOpenReplyExt)> {
    let value = serde_json::from_slice(body)?;
    let authority = RouteOpenReplyExt::read_from(&value)?;
    Ok((serde_json::from_value(value)?, authority))
}
