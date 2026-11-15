use super::*;
use subc_control::{ScopeWatchRequest, ScopeWatchResponse};

const MAX_SCOPE_WATCH_SELECTORS: usize = 256;
const MAX_SCOPE_WATCHES_PER_CONNECTION: usize = 16;

impl ControlHandler {
    pub(super) fn handle_scope_watch(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let Ok(mut request) = serde_json::from_slice::<ScopeWatchRequest>(&frame.body) else {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_control_body",
                "malformed scope.watch",
            )?]);
        };
        if request.scopes.len() > MAX_SCOPE_WATCH_SELECTORS {
            return Ok(vec![control_error_frame(
                &frame,
                "scope_watch_limit_exceeded",
                "scope.watch accepts at most 256 selectors",
            )?]);
        }
        let Some(registration) = self
            .registry
            .get_module_by_connection(ctx.connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "scope.watch requires the module's registered connection",
            )?]);
        };
        let mut streams = self.scope_streams.lock().unwrap_or_else(|p| p.into_inner());
        streams.retain(|_, task| !task.is_finished());
        let key = (ctx.connection_id, frame.header.corr);
        if streams.contains_key(&key) {
            return Ok(vec![control_error_frame(
                &frame,
                "duplicate_subscription",
                "scope.watch correlation id already in use",
            )?]);
        }
        if streams
            .keys()
            .filter(|(connection, _)| *connection == ctx.connection_id)
            .count()
            >= MAX_SCOPE_WATCHES_PER_CONNECTION
        {
            return Ok(vec![control_error_frame(
                &frame,
                "scope_watch_limit_exceeded",
                "at most 16 scope watches may be open per connection",
            )?]);
        }
        // A request denotes a set; repeated selectors do not create extra events.
        let mut seen = Vec::new();
        request.scopes.retain(|selector| {
            if seen.contains(selector) {
                false
            } else {
                seen.push(selector.clone());
                true
            }
        });
        let (states, mut receiver) = self
            .scopes
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .watch(request.scopes, registration.manifest.capabilities.as_ref());
        let sink = ctx.egress.clone();
        let task = tokio::spawn(async move {
            let build = |ty, body| {
                Frame::build_with_version(
                    response_version(&frame),
                    ty,
                    control_flags(),
                    0,
                    0,
                    frame.header.corr,
                    body,
                )
            };
            let Ok(body) = serde_json::to_vec(&ScopeWatchResponse::new(states)) else {
                return;
            };
            let Ok(first) = build(FrameType::Response, body) else {
                return;
            };
            if sink.send(first).await.is_err() {
                return;
            }
            let mut delivered = 0;
            while receiver.changed().await.is_ok() {
                loop {
                    // Writers latch events without waiting for this connection.
                    // Reserve first so all pending endings survive backpressure.
                    let Ok(permit) = sink.reserve_owned().await else {
                        return;
                    };
                    let event = receiver.borrow_and_update().get(delivered).cloned();
                    let Some(event) = event else {
                        break;
                    };
                    let Ok(body) = serde_json::to_vec(&event) else {
                        return;
                    };
                    let Ok(next) = build(FrameType::StreamData, body) else {
                        return;
                    };
                    if permit.send(next) {
                        return;
                    }
                    delivered += 1;
                }
            }
        });
        streams.insert(key, task.abort_handle());
        Ok(Vec::new())
    }

    pub(super) fn cancel_scope_subscription(&self, connection: ConnectionId, corr: u64) -> bool {
        if let Some(task) = self
            .scope_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(connection, corr))
        {
            task.abort();
            true
        } else {
            false
        }
    }

    pub(super) fn close_scope_subscriptions(&self, connection: ConnectionId) {
        self.scope_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(owner, _), task| {
                if *owner == connection {
                    task.abort();
                    false
                } else {
                    true
                }
            });
    }
}
