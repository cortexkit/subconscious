//! Test-gated walking skeleton for route-local request dispatch.
//!
//! This module is deliberately excluded from default builds. It proves the concurrency
//! mechanism against the real frame, sink, and credit types without changing daemon routing.
//! Run the model-checked test with
//! `cargo test -p subc-core dispatch_spike --features loom`; the package build script scopes
//! `cfg(loom)` to this crate because a workspace-wide setting disables Tokio networking.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
};

use subc_protocol::Frame;
use tokio::sync::{mpsc, Notify};

use crate::{forwarding::ChannelFlow, router::FrameSink};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Open,
    Closing,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotState {
    Queued,
    Sending { cancelled: bool },
    Delivered,
}

#[derive(Debug)]
struct Slot {
    frame: Frame,
    state: SlotState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushOutcome {
    Admitted,
    DuplicateCorr,
    Rejected(Admission),
    Backpressure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelDecision {
    SynthesizeCancelled,
    DeferredToDrain,
    ForwardToModule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitDecision {
    Proceed(Frame),
    RollbackCancelled,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TeardownKind {
    None,
    Reloading,
    Goodbye,
    ConnectionClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyntheticKind {
    Cancelled,
    ModuleReloading,
    BackendError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyntheticTerminal {
    pub corr: u64,
    pub kind: SyntheticKind,
}

/// The synchronized decision core. Callers hold the sole route-inbox mutex.
#[derive(Debug)]
pub(crate) struct RouteInbox {
    admission: Admission,
    queue: VecDeque<u64>,
    slots: HashMap<u64, Slot>,
    depth_cap: usize,
    teardown: TeardownKind,
}

impl RouteInbox {
    pub(crate) fn new(depth_cap: usize) -> Self {
        Self {
            admission: Admission::Open,
            queue: VecDeque::new(),
            slots: HashMap::new(),
            depth_cap,
            teardown: TeardownKind::None,
        }
    }

    pub(crate) fn push_request(&mut self, corr: u64, frame: Frame) -> PushOutcome {
        if self.slots.contains_key(&corr) {
            return PushOutcome::DuplicateCorr;
        }
        if self.admission != Admission::Open {
            return PushOutcome::Rejected(self.admission);
        }
        if self.slots.len() >= self.depth_cap {
            return PushOutcome::Backpressure;
        }

        self.slots.insert(
            corr,
            Slot {
                frame,
                state: SlotState::Queued,
            },
        );
        self.queue.push_back(corr);
        PushOutcome::Admitted
    }

    /// Atomically removes the FIFO head and marks it Sending under the same lock.
    pub(crate) fn pop_for_dispatch(&mut self) -> Option<u64> {
        let corr = self.queue.pop_front()?;
        let Some(slot) = self.slots.get_mut(&corr) else {
            // A queued cancellation leaves one O(1) tombstone. The drain discards at most
            // one tombstone per call instead of scanning or indexing a missing slot.
            return None;
        };
        if slot.state != SlotState::Queued {
            return None;
        }
        slot.state = SlotState::Sending { cancelled: false };
        Some(corr)
    }

    pub(crate) fn on_cancel(&mut self, corr: u64) -> CancelDecision {
        let Some(state) = self.slots.get(&corr).map(|slot| slot.state) else {
            return CancelDecision::ForwardToModule;
        };
        match state {
            SlotState::Queued => {
                self.slots.remove(&corr);
                CancelDecision::SynthesizeCancelled
            }
            SlotState::Sending { .. } => {
                if let Some(slot) = self.slots.get_mut(&corr) {
                    slot.state = SlotState::Sending { cancelled: true };
                }
                CancelDecision::DeferredToDrain
            }
            SlotState::Delivered => CancelDecision::ForwardToModule,
        }
    }

    /// Commits ownership to the terminal path before the module send can observe the frame.
    pub(crate) fn commit_delivered(&mut self, corr: u64) -> CommitDecision {
        let Some(state) = self.slots.get(&corr).map(|slot| slot.state) else {
            return CommitDecision::Missing;
        };
        match state {
            SlotState::Sending { cancelled: true } => {
                self.slots.remove(&corr);
                CommitDecision::RollbackCancelled
            }
            SlotState::Sending { cancelled: false } if self.admission != Admission::Closed => {
                let slot = self
                    .slots
                    .get_mut(&corr)
                    .expect("slot existence was checked under the same mutex");
                slot.state = SlotState::Delivered;
                CommitDecision::Proceed(slot.frame.clone())
            }
            SlotState::Sending { cancelled: false } => {
                self.slots.remove(&corr);
                CommitDecision::RollbackCancelled
            }
            SlotState::Queued | SlotState::Delivered => CommitDecision::Missing,
        }
    }

    /// Returns true only for the first terminal of an outstanding delivered request.
    pub(crate) fn on_terminal(&mut self, corr: u64) -> bool {
        if matches!(
            self.slots.get(&corr).map(|slot| slot.state),
            Some(SlotState::Delivered)
        ) {
            self.slots.remove(&corr);
            true
        } else {
            false
        }
    }

    pub(crate) fn begin_closing(&mut self) {
        if self.admission == Admission::Open {
            self.admission = Admission::Closing;
        }
    }

    /// Closes admission and removes queued slots; Sending/Delivered ownership stays explicit.
    pub(crate) fn finish_closed(&mut self) -> Vec<u64> {
        self.admission = Admission::Closed;
        let mut queued = Vec::new();
        while let Some(corr) = self.queue.pop_front() {
            if matches!(
                self.slots.get(&corr).map(|slot| slot.state),
                Some(SlotState::Queued)
            ) {
                self.slots.remove(&corr);
                queued.push(corr);
            }
        }
        queued
    }

    fn set_teardown(&mut self, kind: TeardownKind) {
        self.teardown = kind;
    }

    fn on_acquire_closed(&mut self, corr: u64) -> Option<TeardownKind> {
        if matches!(
            self.slots.get(&corr).map(|slot| slot.state),
            Some(SlotState::Sending { .. })
        ) {
            self.slots.remove(&corr);
            Some(self.teardown)
        } else {
            None
        }
    }

    fn queue_is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn admission(&self) -> Admission {
        self.admission
    }

    #[cfg(test)]
    fn slot_count(&self) -> usize {
        self.slots.len()
    }

    #[cfg(test)]
    fn state(&self, corr: u64) -> Option<SlotState> {
        self.slots.get(&corr).map(|slot| slot.state)
    }
}

trait CreditRelease {
    fn release_credit(&self);
}

impl CreditRelease for Arc<ChannelFlow> {
    fn release_credit(&self) {
        self.release();
    }
}

/// One acquired credit. Dropping rolls back; committing transfers release to a terminal slot.
struct CreditToken<F: CreditRelease> {
    flow: F,
    armed: bool,
}

impl<F: CreditRelease> CreditToken<F> {
    fn new(flow: F) -> Self {
        Self { flow, armed: true }
    }

    fn commit(mut self) {
        self.armed = false;
    }
}

impl<F: CreditRelease> Drop for CreditToken<F> {
    fn drop(&mut self) {
        if self.armed {
            self.flow.release_credit();
        }
    }
}

/// Thin async wrapper around [`RouteInbox`]. It is not wired into the daemon.
#[derive(Debug)]
pub(crate) struct RouteDispatcher {
    inbox: Mutex<RouteInbox>,
    notify: Notify,
    flow: Arc<ChannelFlow>,
    module_sink: FrameSink,
    synthetic_sink: mpsc::UnboundedSender<SyntheticTerminal>,
}

impl RouteDispatcher {
    pub(crate) fn new(
        depth_cap: usize,
        flow: Arc<ChannelFlow>,
        module_sink: FrameSink,
        synthetic_sink: mpsc::UnboundedSender<SyntheticTerminal>,
    ) -> Self {
        Self {
            inbox: Mutex::new(RouteInbox::new(depth_cap)),
            notify: Notify::new(),
            flow,
            module_sink,
            synthetic_sink,
        }
    }

    pub(crate) fn push_request(&self, corr: u64, frame: Frame) -> PushOutcome {
        let outcome = self
            .inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .push_request(corr, frame);
        if outcome == PushOutcome::Admitted {
            self.notify.notify_one();
        }
        outcome
    }

    pub(crate) fn on_cancel(&self, corr: u64) -> CancelDecision {
        self.inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .on_cancel(corr)
    }

    pub(crate) fn begin_closing(&self, kind: TeardownKind) {
        let mut inbox = self.inbox.lock().expect("route inbox mutex poisoned");
        inbox.set_teardown(kind);
        inbox.begin_closing();
        drop(inbox);
        self.notify.notify_one();
    }

    pub(crate) fn finish_closed(&self) -> Vec<u64> {
        self.inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .finish_closed()
    }

    pub(crate) fn close_flow(&self, kind: TeardownKind) {
        self.begin_closing(kind);
        self.flow.close();
    }

    pub(crate) fn terminal_from_module(&self, corr: u64) -> bool {
        let releases = self
            .inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .on_terminal(corr);
        if releases {
            self.flow.release();
        }
        releases
    }

    fn synthesize(&self, corr: u64, kind: SyntheticKind) {
        let _ = self.synthetic_sink.send(SyntheticTerminal { corr, kind });
    }

    pub(crate) async fn drain(self: Arc<Self>) {
        loop {
            let notified = self.notify.notified();
            let next = {
                let mut inbox = self.inbox.lock().expect("route inbox mutex poisoned");
                let corr = inbox.pop_for_dispatch();
                let has_more_queue_entries = !inbox.queue_is_empty();
                let should_exit = inbox.admission() != Admission::Open
                    && corr.is_none()
                    && !has_more_queue_entries;
                (corr, has_more_queue_entries, should_exit)
            };

            let Some(corr) = next.0 else {
                if next.2 {
                    return;
                }
                if next.1 {
                    continue;
                }
                notified.await;
                continue;
            };

            if self.flow.acquire().await.is_err() {
                let kind = self
                    .inbox
                    .lock()
                    .expect("route inbox mutex poisoned")
                    .on_acquire_closed(corr);
                match kind {
                    Some(TeardownKind::Reloading) => {
                        self.synthesize(corr, SyntheticKind::ModuleReloading);
                    }
                    Some(TeardownKind::Goodbye | TeardownKind::ConnectionClose) => {}
                    Some(TeardownKind::None) => {
                        self.synthesize(corr, SyntheticKind::BackendError);
                    }
                    None => {}
                }
                continue;
            }

            let token = CreditToken::new(Arc::clone(&self.flow));
            let decision = self
                .inbox
                .lock()
                .expect("route inbox mutex poisoned")
                .commit_delivered(corr);
            match decision {
                CommitDecision::Proceed(frame) => {
                    token.commit();
                    if self.module_sink.send(frame).await.is_err()
                        && self.terminal_from_module(corr)
                    {
                        self.synthesize(corr, SyntheticKind::BackendError);
                    }
                }
                CommitDecision::RollbackCancelled => {
                    drop(token);
                    self.synthesize(corr, SyntheticKind::Cancelled);
                }
                CommitDecision::Missing => {
                    drop(token);
                    self.synthesize(corr, SyntheticKind::BackendError);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SpikeRouteKey {
    pub connection: u64,
    pub channel: u16,
}

/// Spike counterpart of `forwarding.rs:52-65`'s `RouteBinding`.
///
/// The weak edge is the concrete path from `router.rs:281-309`'s module terminal arm back to
/// the route inbox. A weak reference avoids a binding/dispatcher ownership cycle.
#[derive(Debug)]
pub(crate) struct SpikeRouteBinding {
    dispatcher: Weak<RouteDispatcher>,
}

impl SpikeRouteBinding {
    pub(crate) fn new(dispatcher: &Arc<RouteDispatcher>) -> Self {
        Self {
            dispatcher: Arc::downgrade(dispatcher),
        }
    }

    pub(crate) fn on_terminal(&self, corr: u64) -> bool {
        self.dispatcher
            .upgrade()
            .is_some_and(|dispatcher| dispatcher.terminal_from_module(corr))
    }
}

/// Spike counterpart of `forwarding.rs`'s `ForwardingInner` dispatcher registry.
///
/// Connection/endpoint teardown enumerates this strong map to close every live dispatcher.
/// The lock order is registry then inbox; no inbox operation calls back into the registry.
#[derive(Debug, Default)]
pub(crate) struct SpikeForwardingInner {
    dispatchers: Mutex<HashMap<SpikeRouteKey, Arc<RouteDispatcher>>>,
}

impl SpikeForwardingInner {
    pub(crate) fn insert(&self, key: SpikeRouteKey, dispatcher: Arc<RouteDispatcher>) {
        self.dispatchers
            .lock()
            .expect("spike forwarding registry mutex poisoned")
            .insert(key, dispatcher);
    }

    pub(crate) fn begin_connection_teardown(&self, connection: u64, kind: TeardownKind) -> usize {
        let dispatchers = self
            .dispatchers
            .lock()
            .expect("spike forwarding registry mutex poisoned");
        let mut count = 0;
        for (key, dispatcher) in dispatchers.iter() {
            if key.connection == connection {
                dispatcher.begin_closing(kind);
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests;
