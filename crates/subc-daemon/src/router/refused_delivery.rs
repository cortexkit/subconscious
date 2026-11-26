use super::{Flags, Frame, FrameType, Priority, RouteBinding, Router};
use tracing::warn;

impl Router {
    /// Ask the module to stop a request whose client delivery was refused. The
    /// CANCEL is best effort and never waits: the module's own reader must not
    /// stall on that module's egress, and the request keeps its credit either
    /// way until the module's terminal arrives.
    pub(super) fn cancel_refused_module_request(&self, route: &RouteBinding, corr: u64) {
        let cancel = Frame::build_with_version(
            route.module_negotiated_ver,
            FrameType::Cancel,
            Flags::new(false, Priority::Passive, false),
            route.module_channel,
            route.module_epoch,
            corr,
            Vec::new(),
        );
        let sent = cancel.map_err(|err| err.to_string()).and_then(|cancel| {
            route
                .module_sink
                .try_send_owned(cancel, || {
                    route.record_forwarded_cancel(corr, &self.counters);
                })
                .map_err(|(_, err)| err.to_string())
        });
        if let Err(error) = sent {
            warn!(
                module_id = %route.module_id,
                module_channel = route.module_channel,
                corr,
                %error,
                "could not send CANCEL for a request whose stream chunk was refused as too large"
            );
        }
    }
}
