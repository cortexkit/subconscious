use super::*;

pub(super) struct Refusals {
    sender: mpsc::Sender<Reply>,
    capacity: Arc<Semaphore>,
    sink: crate::router::FrameSink,
    counters: DaemonCounters,
    module_id: Option<String>,
    registry: Arc<crate::registry::Registry>,
}

struct Reply {
    frame: Frame,
    until: tokio::time::Instant,
    _permit: OwnedSemaphorePermit,
}

impl Refusals {
    pub(super) fn new(
        sink: crate::router::FrameSink,
        counters: DaemonCounters,
        module_id: Option<String>,
        registry: Arc<crate::registry::Registry>,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Reply>(IN_FLIGHT);
        let writer = sink.clone();
        let dropped = counters.clone();
        let module = module_id.clone();
        let registrations = Arc::clone(&registry);
        tokio::spawn(async move {
            while let Some(reply) = receiver.recv().await {
                if !matches!(
                    tokio::time::timeout_at(reply.until, writer.send(reply.frame)).await,
                    Ok(Ok(()))
                ) {
                    dropped
                        .increment_agent_command_refusal_dropped(module.as_deref(), &registrations);
                }
            }
        });
        Self {
            sender,
            capacity: Arc::new(Semaphore::new(IN_FLIGHT)),
            sink,
            counters,
            module_id,
            registry,
        }
    }

    pub(super) fn send(&self, frame: Frame) {
        if self.sink.try_send(frame.clone()).is_ok() {
            return;
        }
        // Even a refusal must not park the shared module reader. The separate
        // sender holds at most four frames including its active timed send.
        if let Ok(permit) = Arc::clone(&self.capacity).try_acquire_owned() {
            if self
                .sender
                .try_send(Reply {
                    frame,
                    until: tokio::time::Instant::now() + REPLY_WINDOW,
                    _permit: permit,
                })
                .is_ok()
            {
                return;
            }
        }
        self.counters
            .increment_agent_command_refusal_dropped(self.module_id.as_deref(), &self.registry);
    }
}
