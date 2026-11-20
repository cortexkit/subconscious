//! Requests whose stream the client could no longer receive.
//!
//! A module stream chunk larger than the client's egress budget ends the
//! client's view of that request with `frame_too_large`, but the module may
//! still be computing it. The request keeps its credit, so a Serial route admits
//! nothing new and a drain keeps waiting, until the module's own terminal frame
//! arrives; every frame of that correlation in between is dropped instead of
//! reaching a client that already saw the request end.

use std::sync::atomic::Ordering;

use super::ChannelFlow;

impl ChannelFlow {
    /// Stop delivering `corr` to the client. Its credit stays held until the
    /// module's terminal frame for it is passed to [`Self::drop_refused_frame`].
    pub(crate) fn refuse_client_delivery(&self, corr: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.refused.insert(corr) {
            self.refused_count.fetch_add(1, Ordering::Release);
        }
    }

    /// Whether a module frame for `corr` must be dropped because the client's
    /// delivery of that request already ended. A terminal frame also ends the
    /// refusal and releases the request's credit. Routes with no refused
    /// request pay one atomic load per frame and take no lock.
    pub(crate) fn drop_refused_frame(&self, corr: u64, terminal: bool) -> bool {
        if self.refused_count.load(Ordering::Acquire) == 0 {
            return false;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.refused.contains(&corr) {
            return false;
        }
        if terminal {
            state.refused.remove(&corr);
            self.refused_count.fetch_sub(1, Ordering::Release);
            drop(state);
            self.release_corr(corr);
        }
        true
    }
}
