//! Forwarding a legacy route's client frames without parking the connection
//! reader on request credit; the queue itself is described in
//! `forwarding_waiting_requests`.

use super::*;
use crate::forwarding::{ChannelFlowClosed, LegacyArrival};

impl ForwardBackend {
    /// Forward, queue, or (only when the route's queue is full) wait to queue
    /// a client frame on a route opened without `busy_refusals`.
    pub(super) async fn offer_to_legacy_route(
        &self,
        frame: Frame,
        route: Arc<RouteBinding>,
    ) -> Result<(), RouterError> {
        let mut frame = frame;
        loop {
            match route.flow.offer_legacy(frame) {
                LegacyArrival::Forward { frame, credited } => {
                    return self.forward_to_module(frame, &route, credited).await;
                }
                LegacyArrival::Queued { start_forwarding } => {
                    if start_forwarding {
                        tokio::spawn(self.clone().forward_waiting(route));
                    }
                    return Ok(());
                }
                LegacyArrival::Full(refused) => {
                    frame = refused;
                    self.forwarding
                        .counters_ref()
                        .observe_reader_wait(
                            || (route.module_id.clone(), principal_label(&route.principal)),
                            "legacy_credit",
                            route.flow.waiting_room(),
                        )
                        .await;
                }
                LegacyArrival::Closed(frame) => {
                    let header = &frame.header;
                    return Err(self.legacy_credit_error(
                        &route,
                        header.channel,
                        header.epoch,
                        header.corr,
                        ChannelFlowClosed,
                    )?);
                }
            }
        }
    }

    /// Move a route's waiting frames to the module in order, each request once
    /// it has credit. A frame that cannot go gets the ERROR the connection
    /// reader would have sent for it. Every wait here also ends when the
    /// route's credit window closes, as it does when the route is released or
    /// its client connection is cleaned up: nothing then reaches the module,
    /// and a client that stopped reading cannot hold this task.
    async fn forward_waiting(self, route: Arc<RouteBinding>) {
        while let Some(frame) = route.flow.next_waiting() {
            let (ty, channel, epoch, corr) = (
                frame.header.ty,
                frame.header.channel,
                frame.header.epoch,
                frame.header.corr,
            );
            let error = tokio::select! {
                biased;
                () = route.flow.closed() => {
                    if ty != FrameType::Request {
                        continue;
                    }
                    match self.legacy_credit_error(&route, channel, epoch, corr, ChannelFlowClosed) {
                        Ok(error) | Err(error) => error,
                    }
                }
                result = self.forward_waiting_frame(frame, &route) => match result {
                    Ok(()) => continue,
                    Err(error) => error,
                },
            };
            let Some(error_frame) = error.to_error_frame() else {
                continue;
            };
            tokio::select! {
                biased;
                () = route.flow.closed() => {
                    // Best effort only once the route is gone: never wait.
                    let _ = route.client_sink.try_send(error_frame);
                }
                _ = route.client_sink.send(error_frame.clone()) => {}
            }
        }
    }

    async fn forward_waiting_frame(
        &self,
        frame: Frame,
        route: &RouteBinding,
    ) -> Result<(), RouterError> {
        let credited = frame.header.ty == FrameType::Request;
        if credited {
            let subscription = frame.header.flags.is_subscription();
            if let Err(closed) = route
                .flow
                .acquire_tagged(frame.header.corr, subscription)
                .await
            {
                let header = &frame.header;
                return Err(self.legacy_credit_error(
                    route,
                    header.channel,
                    header.epoch,
                    header.corr,
                    closed,
                )?);
            }
        }
        self.forward_to_module(frame, route, credited).await
    }

    /// The error for a request whose route closed its credit window before
    /// the request could be forwarded.
    fn legacy_credit_error(
        &self,
        route: &RouteBinding,
        channel: u16,
        epoch: u32,
        corr: u64,
        err: ChannelFlowClosed,
    ) -> Result<RouterError, RouterError> {
        self.forwarding.warn_frame_drop(
            route.client_connection_id,
            channel,
            epoch,
            "client_to_module",
            "endpoint_gone",
        );
        // `module_reloading` here is answered BEFORE the frame is forwarded,
        // so the module never sees the request and callers may re-dispatch it
        // after reopening the route. That is a wire guarantee documented on
        // `error_codes::MODULE_RELOADING`; do not emit this code for a request
        // that already reached the module's queue.
        if self
            .forwarding
            .endpoint_is_draining(route.module_endpoint)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(RouterError::route_error_with_epoch(
                channel,
                epoch,
                corr,
                "module_reloading",
                format!("module endpoint for route channel {channel} is reloading"),
            ));
        }
        Ok(RouterError::backend_with_epoch(
            channel,
            epoch,
            corr,
            format!("{err} for route channel {channel}"),
        ))
    }
}
