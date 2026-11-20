//! Client frames waiting for a legacy route's request credit.
//!
//! A route opened without `busy_refusals` makes a request wait for credit
//! instead of refusing it. That wait used to park the client connection's
//! reader, so nothing the client sent afterwards could be read: not a CANCEL,
//! not another route's frames, and not the answer to a reverse request the
//! module asked while running the request that holds the credit. On a Serial
//! route that answer was then needed to free the very credit the reader was
//! waiting for. A request that cannot take a credit at once now waits here
//! instead, up to [`WAITING_REQUESTS_PER_ROUTE`] frames per route, and a
//! forwarding task moves the queue to the module in order as credit frees.
//!
//! What passes the queue, and what does not:
//! - A reply (`RESPONSE` or `ERROR`) to one of the module's reverse requests
//!   never queues and never takes credit.
//! - Any other frame whose correlation has no request waiting here or being
//!   sent by the forwarding task (a CANCEL or stream frame for a request the
//!   module already has) is forwarded at once.
//! - A frame for the correlation of a waiting request, typically its CANCEL,
//!   is placed directly behind that request's frames, ahead of later
//!   requests, so it reaches the module right after its request and takes no
//!   credit of its own.
//! - A request waits behind every frame already queued.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;

use subc_protocol::{Frame, FrameType};

use super::ChannelFlow;

/// How many client frames one legacy route holds waiting for credit before
/// the connection reader itself waits for room, as it once waited for credit.
pub(crate) const WAITING_REQUESTS_PER_ROUTE: usize = 64;

#[derive(Debug, Default)]
pub(super) struct WaitingRequests {
    queued: VecDeque<Frame>,
    /// Set while a forwarding task owns the queue, from the first queued
    /// frame until it finds the queue empty after sending its last frame.
    forwarding: bool,
    /// The correlation of the frame that task took last and may still be
    /// sending, so frames for it keep queuing behind it.
    sending: Option<u64>,
}

impl WaitingRequests {
    /// Frames waiting, counting the one the forwarding task is still sending.
    fn len(&self) -> usize {
        self.queued.len() + usize::from(self.sending.is_some())
    }

    /// Where a frame for `corr` goes so that it follows that correlation's
    /// waiting frames and nothing else: right after the last of them, or at
    /// the front when only the frame being sent has it. `None` when nothing
    /// for `corr` is waiting.
    fn slot_behind(&self, corr: u64) -> Option<usize> {
        let last = self
            .queued
            .iter()
            .rposition(|frame| frame.header.corr == corr);
        match last {
            Some(index) => Some(index + 1),
            None => (self.sending == Some(corr)).then_some(0),
        }
    }
}

/// Where a client frame for a legacy route goes.
#[derive(Debug)]
pub(crate) enum LegacyArrival {
    /// Nothing waits ahead of it: forward it now. A request has already
    /// taken its credit when `credited` is set.
    Forward { frame: Frame, credited: bool },
    /// Queued behind earlier frames. When `start_forwarding` is set, the
    /// caller must start the task that drains the queue.
    Queued { start_forwarding: bool },
    /// The queue is full: wait for [`ChannelFlow::waiting_room`], then offer
    /// the frame again.
    Full(Frame),
    /// The route's credit window is closed, so the request cannot be sent.
    Closed(Frame),
}

impl ChannelFlow {
    /// Decide where a client frame for this legacy route goes, taking a
    /// request's credit at once when nothing is waiting ahead of it.
    pub(crate) fn offer_legacy(&self, frame: Frame) -> LegacyArrival {
        let forward = |frame| LegacyArrival::Forward {
            frame,
            credited: false,
        };
        match frame.header.ty {
            // Answers to the module's reverse requests take no credit and
            // are what a waiting request may itself be waiting for.
            FrameType::Response | FrameType::Error => return forward(frame),
            FrameType::Request => {}
            // Nothing waits, so nothing can need to stay ahead of this frame.
            // The connection reader is the only producer, so an empty queue
            // seen here stays empty until it queues something itself.
            _ if self.waiting_len.load(Ordering::Acquire) == 0 => return forward(frame),
            _ => {}
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let corr = frame.header.corr;
        let slot = if frame.header.ty == FrameType::Request {
            if state.closed {
                return LegacyArrival::Closed(frame);
            }
            let idle = state.waiting.queued.is_empty() && !state.waiting.forwarding;
            if idle {
                if let Ok(permit) = self.sem.try_acquire() {
                    permit.forget();
                    let subscription = frame.header.flags.is_subscription();
                    state.credits.acquire(corr, subscription, false);
                    return LegacyArrival::Forward {
                        frame,
                        credited: true,
                    };
                }
            }
            state.waiting.queued.len()
        } else {
            match state.waiting.slot_behind(corr) {
                Some(slot) => slot,
                None => return forward(frame),
            }
        };
        if state.waiting.len() >= WAITING_REQUESTS_PER_ROUTE {
            return LegacyArrival::Full(frame);
        }
        state.waiting.queued.insert(slot, frame);
        let start_forwarding = !state.waiting.forwarding;
        state.waiting.forwarding = true;
        self.waiting_len
            .store(state.waiting.len(), Ordering::Release);
        LegacyArrival::Queued { start_forwarding }
    }

    /// The forwarding task's next frame, or `None` once the queue is empty,
    /// at which point the task must end: a later queued frame starts another.
    pub(crate) fn next_waiting(&self) -> Option<Frame> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next = state.waiting.queued.pop_front();
        state.waiting.sending = next.as_ref().map(|frame| frame.header.corr);
        state.waiting.forwarding = next.is_some();
        self.waiting_len
            .store(state.waiting.len(), Ordering::Release);
        drop(state);
        self.waiting_room.notify_waiters();
        next
    }

    /// Wait until this route's queue of waiting frames has room.
    pub(crate) async fn waiting_room(&self) {
        loop {
            let room = self.waiting_room.notified();
            tokio::pin!(room);
            room.as_mut().enable();
            if self.waiting_len.load(Ordering::Acquire) < WAITING_REQUESTS_PER_ROUTE {
                return;
            }
            room.await;
        }
    }

    /// Resolve once the route's credit window is closed, as it is when the
    /// route is released or its client connection is cleaned up.
    pub(crate) async fn closed(&self) {
        loop {
            let woken = self.waiting_room.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if self.is_closed() {
                return;
            }
            woken.await;
        }
    }

    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed
    }
}
