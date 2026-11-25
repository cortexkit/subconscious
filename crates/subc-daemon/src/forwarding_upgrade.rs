use std::{
    future::{poll_fn, Future},
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{oneshot, watch, Notify};

use super::{CloseReason, ForwardingTable, FrameSink};
use crate::control::upgrade_in_place::Admission;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Default)]
pub(crate) struct CutoverCount(Arc<watch::Sender<usize>>);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl CutoverCount {
    pub(crate) fn enter(&self) -> CutoverTicket {
        self.0.send_modify(|count| *count += 1);
        CutoverTicket(self.0.clone())
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<usize> {
        self.0.subscribe()
    }
}

#[derive(Debug)]
pub(crate) struct CutoverTicket(Arc<watch::Sender<usize>>);

impl Drop for CutoverTicket {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}

#[derive(Debug, Default)]
pub(crate) struct CutoverTasks {
    active: AtomicUsize,
    fenced: AtomicBool,
    changed: Notify,
}

pub(crate) struct CutoverTask<T> {
    owner: T,
    tasks: fn(&T) -> &CutoverTasks,
}

impl<T> CutoverTask<T> {
    /// Adopt the router/backend the task was already going to own. No extra
    /// shared allocation or reference count is needed to track its lifetime.
    pub(crate) fn new(owner: T, tasks: fn(&T) -> &CutoverTasks) -> Self {
        tasks(&owner).active.fetch_add(1, Ordering::Relaxed);
        Self { owner, tasks }
    }

    pub(crate) fn owner(&self) -> &T {
        &self.owner
    }
}

impl<T> Drop for CutoverTask<T> {
    fn drop(&mut self) {
        let tasks = (self.tasks)(&self.owner);
        let previous = tasks.active.fetch_sub(1, Ordering::SeqCst);
        if tasks.fenced.load(Ordering::SeqCst) && previous == 1 {
            tasks.changed.notify_waiters();
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl CutoverTasks {
    fn set_fenced(&self, fenced: bool) {
        self.fenced.store(fenced, Ordering::SeqCst);
    }

    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub(crate) async fn wait_until_idle(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Arm before inspecting the count. The SC flag and count also
            // cover a last task exiting while the fence is being raised.
            changed.as_mut().enable();
            if self.active() == 0 {
                return;
            }
            changed.await;
        }
    }
}

#[derive(Debug)]
enum Signal {
    Close(CloseReason),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Phase {
        phase: Admission,
        reader: Option<CutoverTicket>,
        next: oneshot::Receiver<Signal>,
    },
}

#[derive(Debug)]
pub(super) struct ConnectionCloseSender {
    tx: Option<oneshot::Sender<Signal>>,
    reader: bool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    closing_reader: Option<CutoverTicket>,
}

#[derive(Debug)]
pub(crate) struct ConnectionCloseReceiver {
    rx: oneshot::Receiver<Signal>,
    phase: Admission,
    reader: Option<CutoverTicket>,
}

pub(crate) struct UpgradeReader<'a> {
    forwarding: &'a ForwardingTable,
    id: crate::ConnectionId,
}

impl Drop for UpgradeReader<'_> {
    fn drop(&mut self) {
        if let Some(entry) = self.forwarding.lock_close_registry().get_mut(&self.id) {
            entry.reader = false;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                entry.closing_reader = None;
            }
        }
    }
}

pub(super) fn close_channel() -> (ConnectionCloseSender, ConnectionCloseReceiver) {
    let (tx, rx) = oneshot::channel();
    (
        ConnectionCloseSender {
            tx: Some(tx),
            reader: false,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            closing_reader: None,
        },
        ConnectionCloseReceiver {
            rx,
            phase: Admission::Open,
            reader: None,
        },
    )
}

impl ConnectionCloseSender {
    pub(super) fn send(&mut self, reason: CloseReason) -> bool {
        let Some(tx) = self.tx.take() else {
            return false;
        };
        let _ = tx.send(Signal::Close(reason));
        true
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn pulse(&mut self, phase: Admission, reader: Option<CutoverTicket>) {
        let Some(sender) = &mut self.tx else {
            // The close signal already owns the one-shot. Keep counting
            // this reader until its read loop has returned.
            self.closing_reader = reader;
            return;
        };
        let (tx, next) = oneshot::channel();
        let old = std::mem::replace(sender, tx);
        let _ = old.send(Signal::Phase {
            phase,
            reader,
            next,
        });
    }
}

impl ConnectionCloseReceiver {
    pub(crate) fn phase(&self) -> Admission {
        self.phase
    }

    pub(crate) fn initialize_phase(&mut self, phase: Admission) {
        self.phase = phase;
    }

    pub(crate) fn acknowledge_reader(&mut self) {
        if self.reader.is_some() {
            drop(self.reader.take());
        }
    }

    fn handle_signal(&mut self, signal: Signal) -> Option<CloseReason> {
        match signal {
            Signal::Close(reason) => Some(reason),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Signal::Phase {
                phase,
                reader,
                next,
            } => {
                self.phase = phase;
                self.reader = reader;
                self.rx = next;
                None
            }
        }
    }

    pub(crate) async fn next_signal(
        &mut self,
    ) -> Result<Option<CloseReason>, oneshot::error::RecvError> {
        poll_fn(|cx| {
            Pin::new(&mut self.rx)
                .poll(cx)
                .map(|event| event.map(|event| self.handle_signal(event)))
        })
        .await
    }

    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Result<CloseReason, oneshot::error::TryRecvError> {
        loop {
            let signal = self.rx.try_recv()?;
            if let Some(reason) = self.handle_signal(signal) {
                return Ok(reason);
            }
        }
    }
}

impl Future for ConnectionCloseReceiver {
    type Output = Result<CloseReason, oneshot::error::RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let signal = match Pin::new(&mut self.rx).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(signal)) => signal,
            };
            // The reader can receive a freeze signal during dispatch, but
            // acknowledges it only after the frame settles or the reader exits.
            if let Some(reason) = self.handle_signal(signal) {
                return Poll::Ready(Ok(reason));
            }
        }
    }
}

impl ForwardingTable {
    pub(crate) fn start_upgrade_reader(
        &self,
        id: crate::ConnectionId,
    ) -> (Admission, UpgradeReader<'_>) {
        let mut connections = self.lock_close_registry();
        if let Some(connection) = connections.get_mut(&id) {
            connection.reader = true;
        }
        (
            self.upgrade_admission.phase(),
            UpgradeReader {
                forwarding: self,
                id,
            },
        )
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn freeze_upgrade_readers(&self) -> CutoverCount {
        self.set_upgrade_admission(Admission::ClientsFrozen);
        let pending = CutoverCount::default();
        for connection in self
            .lock_close_registry()
            .values_mut()
            .filter(|connection| connection.reader)
        {
            connection.pulse(Admission::ClientsFrozen, Some(pending.enter()));
        }
        pending
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn set_upgrade_admission(&self, phase: Admission) {
        // Registration takes the write side and installs the current fence
        // before making its sink visible. Fence existing sinks before the
        // connection readers start counting requests refused for the upgrade.
        let mut inner = self.inner.write().unwrap_or_else(|p| p.into_inner());
        inner.upgrade_fenced = phase.fenced();
        for module in inner
            .modules_by_id
            .values()
            .chain(inner.candidates_by_id.values())
            .chain(inner.superseded_endpoints.values())
        {
            module.sink.fence_forwarded(phase.fenced());
        }
        self.upgrade_admission.set(phase);
        self.upgrade_refusals.tasks.set_fenced(phase.fenced());
        if !phase.fenced() {
            self.route_bind_concurrency.clear_upgrade_producers();
            for connection in self
                .lock_close_registry()
                .values_mut()
                .filter(|connection| connection.reader)
            {
                connection.pulse(Admission::Open, None);
            }
        }
    }

    pub(super) fn fence_new_module_sink(&self, sink: &FrameSink) {
        if self.upgrade_admission.phase().fenced() {
            sink.fence_forwarded(true);
        }
    }
}
