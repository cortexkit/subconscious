//! Test-gated walking skeleton for route-local request dispatch.
//!
//! This module is deliberately excluded from default builds. It proves the concurrency
//! mechanism against the real frame, sink, and credit types without changing daemon routing.
//! Run the model-checked test with
//! `cargo test -p subc-core dispatch_spike --features loom`; the package build script scopes
//! `cfg(loom)` to this crate because a workspace-wide setting disables Tokio networking.
//!
//! # Why it is still dormant, and what would promote it
//!
//! It exists to answer one question ahead of the work: can cancellation reach a request
//! that is already queued behind a saturated serial route? Today it cannot -- the connection
//! read loop blocks on the route's credit semaphore, so a CANCEL for a queued request waits
//! behind the request it is trying to cancel. This skeleton demonstrates the fix (per-route
//! bounded queues with a single drain owner) without touching daemon routing, so the
//! mechanism could be model-checked before anything depended on it.
//!
//! It stays dormant because the starvation it addresses has not been observed in production;
//! the cost of the redesign is currently larger than the harm. That is a judgement about
//! today's evidence, not a permanent verdict -- a reproducible case of a cancel failing to
//! land on a busy serial route is what would justify wiring it, and the mechanism is proven
//! and waiting when that arrives.
//!
//! Recorded because dormancy and abandonment look identical from the outside: a reader
//! finding unreferenced code cannot tell whether it is waiting or forgotten, and deleting
//! a proven mechanism is expensive to undo.
//!
//! # Single-writer invariant
//!
//! [`RouteDrain`] owns the only `FrameSink` for a route and is consumed by one drain task.
//! Read-loop actors can only enqueue cancellation intent through [`RouteDispatcher`]; they can
//! never write module-bound frames directly. Therefore the drain's Request send completes before
//! it dequeues and sends a CANCEL for that Delivered correlation.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

use subc_protocol::{Frame, FrameType};
use tokio::sync::{mpsc, Notify, Semaphore};

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
    QueuedForDrain,
    DrainStopped,
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

/// Thin synchronized wrapper around [`RouteInbox`]. It is not wired into the daemon.
#[derive(Debug)]
pub(crate) struct RouteDispatcher {
    inbox: Mutex<RouteInbox>,
    notify: Notify,
    flow: Arc<ChannelFlow>,
    cancel_tx: mpsc::UnboundedSender<Frame>,
    synthetic_sink: mpsc::UnboundedSender<SyntheticTerminal>,
}

/// Consumed by one task, making that task the route's only module-sink writer.
#[derive(Debug)]
pub(crate) struct RouteDrain {
    dispatcher: Arc<RouteDispatcher>,
    module_sink: FrameSink,
    cancel_rx: mpsc::UnboundedReceiver<Frame>,
    send_gate: Option<Arc<SendGate>>,
}

/// Test synchronization hook that parks the first Request before it reaches the real sink.
#[derive(Debug)]
pub(crate) struct SendGate {
    armed: AtomicBool,
    entered: Notify,
    release: Semaphore,
}

impl SendGate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(true),
            entered: Notify::new(),
            release: Semaphore::new(0),
        })
    }

    pub(crate) async fn wait_until_parked(&self) {
        self.entered.notified().await;
    }

    pub(crate) fn release(&self) {
        self.release.add_permits(1);
    }

    async fn park_if_first_request(&self, frame_type: FrameType) {
        if frame_type == FrameType::Request && self.armed.swap(false, Ordering::AcqRel) {
            self.entered.notify_one();
            let permit = self
                .release
                .acquire()
                .await
                .expect("test send gate must remain open");
            permit.forget();
        }
    }
}

impl RouteDispatcher {
    pub(crate) fn new(
        depth_cap: usize,
        flow: Arc<ChannelFlow>,
        module_sink: FrameSink,
        synthetic_sink: mpsc::UnboundedSender<SyntheticTerminal>,
    ) -> (Arc<Self>, RouteDrain) {
        let (cancel_tx, cancel_rx) = mpsc::unbounded_channel();
        let dispatcher = Arc::new(Self {
            inbox: Mutex::new(RouteInbox::new(depth_cap)),
            notify: Notify::new(),
            flow,
            cancel_tx,
            synthetic_sink,
        });
        let drain = RouteDrain {
            dispatcher: Arc::clone(&dispatcher),
            module_sink,
            cancel_rx,
            send_gate: None,
        };
        (dispatcher, drain)
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

    /// Records cancellation synchronously; module forwarding is always delegated to the drain.
    pub(crate) fn submit_cancel(&self, frame: Frame) -> CancelDecision {
        debug_assert_eq!(frame.header.ty, FrameType::Cancel);
        let corr = frame.header.corr;
        let decision = self
            .inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .on_cancel(corr);
        match decision {
            CancelDecision::SynthesizeCancelled => {
                self.synthesize(corr, SyntheticKind::Cancelled);
                CancelDecision::SynthesizeCancelled
            }
            CancelDecision::DeferredToDrain => CancelDecision::DeferredToDrain,
            CancelDecision::ForwardToModule => {
                if self.cancel_tx.send(frame).is_ok() {
                    CancelDecision::QueuedForDrain
                } else {
                    CancelDecision::DrainStopped
                }
            }
            CancelDecision::QueuedForDrain | CancelDecision::DrainStopped => decision,
        }
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
}

impl RouteDrain {
    pub(crate) fn with_send_gate(mut self, gate: Arc<SendGate>) -> Self {
        self.send_gate = Some(gate);
        self
    }

    /// The sole call site that writes a route frame to the module sink.
    async fn send_to_module(&self, frame: Frame) -> bool {
        if let Some(gate) = &self.send_gate {
            gate.park_if_first_request(frame.header.ty).await;
        }
        self.module_sink.send(frame).await.is_ok()
    }

    pub(crate) async fn run(mut self) {
        loop {
            // A cancel queued while the preceding Request send was blocked must be emitted
            // before the drain starts another request-credit acquisition.
            if let Ok(cancel) = self.cancel_rx.try_recv() {
                let _ = self.send_to_module(cancel).await;
                continue;
            }

            let notified = self.dispatcher.notify.notified();
            let next = {
                let mut inbox = self
                    .dispatcher
                    .inbox
                    .lock()
                    .expect("route inbox mutex poisoned");
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
                tokio::select! {
                    cancel = self.cancel_rx.recv() => {
                        if let Some(cancel) = cancel {
                            let _ = self.send_to_module(cancel).await;
                        }
                    }
                    () = notified => {}
                }
                continue;
            };

            // A later Request may be waiting for credit held by the cancel target. Keep
            // forwarding cancels while acquire is blocked so the target can settle and return
            // the credit that lets the later Request proceed.
            let acquired = loop {
                tokio::select! {
                    cancel = self.cancel_rx.recv() => {
                        if let Some(cancel) = cancel {
                            let _ = self.send_to_module(cancel).await;
                        }
                    }
                    acquired = self.dispatcher.flow.acquire() => break acquired,
                }
            };

            if acquired.is_err() {
                let kind = self
                    .dispatcher
                    .inbox
                    .lock()
                    .expect("route inbox mutex poisoned")
                    .on_acquire_closed(corr);
                match kind {
                    Some(TeardownKind::Reloading) => {
                        self.dispatcher
                            .synthesize(corr, SyntheticKind::ModuleReloading);
                    }
                    Some(TeardownKind::Goodbye | TeardownKind::ConnectionClose) => {}
                    Some(TeardownKind::None) => {
                        self.dispatcher
                            .synthesize(corr, SyntheticKind::BackendError);
                    }
                    None => {}
                }
                continue;
            }

            let token = CreditToken::new(Arc::clone(&self.dispatcher.flow));
            let decision = self
                .dispatcher
                .inbox
                .lock()
                .expect("route inbox mutex poisoned")
                .commit_delivered(corr);
            match decision {
                CommitDecision::Proceed(frame) => {
                    token.commit();
                    if !self.send_to_module(frame).await
                        && self.dispatcher.terminal_from_module(corr)
                    {
                        self.dispatcher
                            .synthesize(corr, SyntheticKind::BackendError);
                    }
                }
                CommitDecision::RollbackCancelled => {
                    drop(token);
                    self.dispatcher.synthesize(corr, SyntheticKind::Cancelled);
                }
                CommitDecision::Missing => {
                    drop(token);
                    self.dispatcher
                        .synthesize(corr, SyntheticKind::BackendError);
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
