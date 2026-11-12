use super::*;
use crate::resource_totals::{CommitFence, Declaration};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Weak,
};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
mod refusals;

const IN_FLIGHT: usize = 4;
const VALIDATION_WINDOW: Duration = Duration::from_millis(200);
const REPLY_WINDOW: Duration = Duration::from_millis(200);

#[derive(Clone, Default)]
pub(crate) struct Queues(Arc<Mutex<HashMap<ConnectionId, Arc<Queue>>>>);

struct Queue {
    sender: mpsc::Sender<Work>,
    capacity: Arc<Semaphore>,
    fences: Mutex<BTreeMap<u64, Arc<CommitFence>>>,
    closed: AtomicBool,
    refusals: refusals::Refusals,
    losses: Arc<crate::resource_totals::declaration_loss::Losses>,
}

struct Work {
    losses: Arc<crate::resource_totals::declaration_loss::Losses>,
    handler: ControlHandler,
    ctx: RouteCtx,
    frame: Frame,
    batch: Declaration,
    fence: Arc<CommitFence>,
    _permit: OwnedSemaphorePermit,
}

impl Queues {
    #[cfg(test)]
    pub(crate) fn fault_for_test(
        &self,
        handler: &ControlHandler,
        ctx: &RouteCtx,
        closed: bool,
        corr: u64,
    ) {
        let queue = self.queue(handler, ctx);
        queue.closed.store(closed, Ordering::Release);
        if !closed {
            queue
                .fences
                .lock()
                .unwrap()
                .insert(corr, Arc::new(CommitFence::default()));
        }
    }
    fn queue(&self, handler: &ControlHandler, ctx: &RouteCtx) -> Arc<Queue> {
        let module_id = handler
            .registry
            .get_module_by_connection(ctx.connection_id)
            .ok()
            .flatten()
            .map(|registration| registration.manifest.module_id);
        {
            let mut queues = self.0.lock().unwrap_or_else(|p| p.into_inner());
            Arc::clone(queues.entry(ctx.connection_id).or_insert_with(|| {
                let (sender, receiver) = mpsc::channel(IN_FLIGHT);
                let queue = Arc::new(Queue {
                    sender,
                    capacity: Arc::new(Semaphore::new(IN_FLIGHT)),
                    fences: Mutex::default(),
                    closed: AtomicBool::new(false),
                    losses: module_id
                        .as_deref()
                        .map(|id| handler.resource_tracker.declaration_loss(id))
                        .unwrap_or_default(),
                    refusals: refusals::Refusals::new(
                        ctx.egress.clone(),
                        handler.counters.clone(),
                        module_id,
                        Arc::clone(&handler.registry),
                    ),
                });
                tokio::spawn(run(Arc::downgrade(&queue), receiver));
                queue
            }))
        }
    }

    pub(crate) fn refuse(&self, handler: &ControlHandler, ctx: &RouteCtx, frame: Frame) {
        self.queue(handler, ctx).refusals.send(frame);
    }

    pub(crate) async fn enqueue(
        &self,
        handler: &ControlHandler,
        ctx: &RouteCtx,
        frame: Frame,
        batch: Declaration,
    ) -> Result<Vec<Frame>, RouterError> {
        if let Some(error) = super::check_batch(
            &frame,
            batch.started.len(),
            batch.finished.len(),
            batch.known.as_ref().map_or(0, Vec::len),
        )? {
            self.refuse(handler, ctx, error);
            return Ok(Vec::new());
        }
        let queue = self.queue(handler, ctx);
        // This supervised reader also carries tool traffic and cancellation.
        // Refuse excess work immediately instead of parking the reader.
        let Ok(permit) = Arc::clone(&queue.capacity).try_acquire_owned() else {
            queue.losses.mark(&batch.started);
            queue.refusals.send(control_error_frame(
                &frame,
                "busy",
                "four agent command declarations are in flight; retry",
            )?);
            return Ok(Vec::new());
        };
        let fence = Arc::new(CommitFence::default());
        {
            let mut fences = queue.fences.lock().unwrap_or_else(|p| p.into_inner());
            if queue.closed.load(Ordering::Acquire) {
                drop(fences);
                queue.losses.mark(&batch.started);
                queue.refusals.send(control_error_frame(
                    &frame,
                    subc_protocol::error_codes::DECLARATION_CLOSED,
                    "declaration queue is closed; retry on a current supervised connection",
                )?);
                return Ok(Vec::new());
            }
            if fences.contains_key(&frame.header.corr) {
                drop(fences);
                queue.losses.mark(&batch.started);
                queue.refusals.send(control_error_frame(
                    &frame,
                    subc_protocol::error_codes::DUPLICATE_CORRELATION,
                    "declaration correlation is already in flight; retry with a fresh correlation",
                )?);
                return Ok(Vec::new());
            }
            fences.insert(frame.header.corr, Arc::clone(&fence));
        }
        let corr = frame.header.corr;
        let refused = frame.clone();
        let sent = queue.sender.try_send(Work {
            losses: Arc::clone(&queue.losses),
            handler: handler.clone(),
            ctx: ctx.clone(),
            frame,
            batch,
            fence,
            _permit: permit,
        });
        if let Err(error) = sent {
            queue.losses.mark(&error.into_inner().batch.started);
            queue
                .fences
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&corr);
            queue.refusals.send(control_error_frame(
                &refused,
                subc_protocol::error_codes::DECLARATION_CLOSED,
                "declaration queue is closed; retry on a current supervised connection",
            )?);
            return Ok(Vec::new());
        }
        Ok(Vec::new())
    }

    pub(crate) fn close(&self, connection: ConnectionId) {
        let Some(queue) = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&connection)
        else {
            return;
        };
        queue.closed.store(true, Ordering::Release);
        queue.capacity.close();
        for fence in queue
            .fences
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            fence.cancel();
        }
    }

    pub(crate) fn cancel(&self, connection: ConnectionId, corr: u64) -> bool {
        if let Some(queue) = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&connection)
        {
            if let Some(fence) = queue
                .fences
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&corr)
            {
                fence.cancel();
                return true;
            }
        }
        false
    }
}

async fn run(queue: Weak<Queue>, mut receiver: mpsc::Receiver<Work>) {
    while let Some(work) = receiver.recv().await {
        let corr = work.frame.header.corr;
        let started = work.batch.started.clone();
        let fence = Arc::clone(&work.fence);
        let reply = validate(
            work.handler,
            work.ctx.connection_id,
            work.frame.clone(),
            work.batch,
            work.fence,
        )
        .await;
        if !fence.committed()
            && reply.as_ref().map_or(true, |frames| {
                frames
                    .iter()
                    .any(|frame| frame.header.ty == FrameType::Error)
            })
        {
            work.losses.mark(&started);
        }
        let frames = reply.unwrap_or_else(|error| {
            control_error_frame(&work.frame, "backend_error", error.to_string())
                .into_iter()
                .collect()
        });
        for frame in frames {
            if !matches!(
                tokio::time::timeout(REPLY_WINDOW, work.ctx.egress.send(frame)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
        if let Some(queue) = queue.upgrade() {
            queue
                .fences
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&corr);
        }
    }
}

async fn validate(
    handler: ControlHandler,
    connection: ConnectionId,
    frame: Frame,
    batch: Declaration,
    fence: Arc<CommitFence>,
) -> Result<Vec<Frame>, RouterError> {
    static SLOTS: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    // Declarations have their own bounded readers, not the list/sampler pool.
    let slots = SLOTS.get_or_init(|| Arc::new(Semaphore::new(4)));
    let deadline = tokio::time::Instant::now() + VALIDATION_WINDOW;
    #[cfg(test)]
    let deadline = handler
        .config_runtime
        .as_ref()
        .and_then(|owner| owner.agent_validation_window_for_test())
        .map_or(deadline, |window| tokio::time::Instant::now() + window);
    let corr = frame.header.corr;
    let timed_out = || RouterError::backend(0, corr, "agent command validation timed out; retry");
    let permit = tokio::time::timeout_at(deadline, Arc::clone(slots).acquire_owned())
        .await
        .map_err(|_| timed_out())?
        .map_err(|_| timed_out())?;
    let worker_fence = Arc::clone(&fence);
    #[cfg(test)]
    let clock = handler
        .supervisor
        .resource_commit_pause_armed_for_test()
        .then(crate::config_gate::runtime::commit_pause::Clock::new);
    #[cfg(test)]
    let worker_clock = clock.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        let _clock = crate::config_gate::runtime::commit_pause::Clock::install(worker_clock);
        let _permit = permit;
        handler.handle_agent_commands_fenced(
            connection,
            frame,
            batch.started,
            batch.finished,
            batch.known,
            &worker_fence,
        )
    });
    #[cfg(test)]
    let result = match clock {
        Some(clock) => clock.timeout(deadline, &mut worker).await,
        None => tokio::time::timeout_at(deadline, &mut worker).await,
    };
    #[cfg(not(test))]
    let result = tokio::time::timeout_at(deadline, &mut worker).await;
    match result {
        Ok(result) => result.map_err(|error| RouterError::backend(0, corr, error.to_string()))?,
        Err(_) if fence.cancel() => Err(timed_out()),
        // The atomic commit decision preceded the timeout. Its short mutation
        // must supply the actual reply, never an error claiming it was cancelled.
        Err(_) => worker
            .await
            .map_err(|error| RouterError::backend(0, corr, error.to_string()))?,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn unarmed_validation_still_expires_at_200ms() {
        assert_eq!(VALIDATION_WINDOW, Duration::from_millis(200));
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(registry.clone());
        assert!(!handler.supervisor.resource_commit_pause_armed_for_test());
        let (locked, wait_locked) = std::sync::mpsc::channel();
        let (release, wait_release) = std::sync::mpsc::channel::<()>();
        let held = registry.clone();
        let holder = std::thread::spawn(move || {
            held.publish_registration_after(ConnectionId::new(906), || {
                locked.send(()).unwrap();
                let _ = wait_release.recv();
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        });
        wait_locked.recv().unwrap();
        let attempted = registry.observe_next_lock_attempt();
        let validation = tokio::spawn(validate(
            handler,
            ConnectionId::new(906),
            frame(1),
            Declaration::default(),
            Arc::new(CommitFence::default()),
        ));
        // Keep virtual time stationary until validation attempts the held registry lock.
        loop {
            if let Ok(acquired) = attempted.try_recv() {
                assert!(!acquired);
                break;
            }
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(199)).await;
        assert!(!validation.is_finished());
        tokio::time::advance(Duration::from_millis(1)).await;
        let error = validation.await.unwrap().unwrap_err();
        assert!(error
            .to_string()
            .contains("agent command validation timed out"));
        drop(release);
        holder.join().unwrap();
    }

    #[tokio::test]
    async fn oversized_batches_are_refused_without_waiting_for_the_loss_metadata_lock() {
        let handler = ControlHandler::new(Arc::new(Registry::default()));
        let (sender, _receiver) = mpsc::channel(1);
        let ctx = RouteCtx {
            connection_id: ConnectionId::new(905),
            egress: FrameSink::new(sender),
        };
        let queue = handler.agent_declarations.queue(&handler, &ctx);
        let losses = Arc::clone(&queue.losses);
        let (ready, wait) = std::sync::mpsc::channel();
        let holder =
            std::thread::spawn(move || losses.hold_for_test(ready, Duration::from_millis(500)));
        wait.recv().unwrap();
        let began = std::time::Instant::now();
        handler
            .agent_declarations
            .enqueue(
                &handler,
                &ctx,
                frame(1),
                Declaration {
                    started: vec![
                        AgentCommandRoot {
                            pid: 42,
                            start_time_ms: 1
                        };
                        257
                    ],
                    ..Declaration::default()
                },
            )
            .await
            .unwrap();
        let elapsed = began.elapsed();
        holder.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(200),
            "an oversized request scanned or waited under the loss lock: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_busy_started_batch_records_missing_coverage_in_module_diagnostics() {
        let registry = Arc::new(Registry::default());
        registry
            .register_with_control_ops(
                subc_protocol::manifest::ModuleManifest::builder("lost-module", "0.1.0")
                    .protocol_ver(1)
                    .build(),
                1,
                ConnectionId::new(901),
                Vec::new(),
            )
            .unwrap();
        let handler = ControlHandler::new(registry);
        let (sender, _receiver) = mpsc::channel(1);
        let ctx = RouteCtx {
            connection_id: ConnectionId::new(901),
            egress: FrameSink::new(sender),
        };
        let queue = handler.agent_declarations.queue(&handler, &ctx);
        let _held = Arc::clone(&queue.capacity)
            .acquire_many_owned(4)
            .await
            .unwrap();
        handler
            .agent_declarations
            .enqueue(
                &handler,
                &ctx,
                frame(1),
                Declaration {
                    started: vec![AgentCommandRoot {
                        pid: 42,
                        start_time_ms: 1,
                    }],
                    ..Declaration::default()
                },
            )
            .await
            .unwrap();
        let loss = handler.resource_tracker.declaration_loss("lost-module");
        assert!(loss.lower());
        let reply = handler.handle_server_describe(frame(2)).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reply[0].body).unwrap();
        assert_eq!(
            body["counters"]["agent_command_declaration_loss_by_module"]["lost-module"]
                ["lost_identities"],
            1
        );
        assert_eq!(
            body["counters"]["agent_command_declaration_loss_by_module"]["lost-module"]["overflow"],
            false
        );
    }
    use crate::router::FrameSink;

    fn frame(corr: u64) -> Frame {
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            b"{}".to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn fifth_declaration_is_refused_without_parking_cancel_or_goodbye() {
        let handler = ControlHandler::new(Arc::new(Registry::default()));
        let queues = handler.agent_declarations.clone();
        let (sender, mut receiver) = mpsc::channel(1);
        let ctx = RouteCtx {
            connection_id: ConnectionId::new(900),
            egress: FrameSink::new(sender),
        };
        ctx.egress.send(frame(0)).await.unwrap();
        for corr in 1..=4 {
            queues
                .enqueue(&handler, &ctx, frame(corr), Declaration::default())
                .await
                .unwrap();
        }
        let fifth = tokio::time::timeout(
            Duration::from_millis(20),
            queues.enqueue(&handler, &ctx, frame(5), Declaration::default()),
        )
        .await
        .expect("the shared reader must never await a declaration permit")
        .unwrap();
        assert!(
            fifth.is_empty(),
            "busy replies use nonblocking egress, not the reader's ordinary reply path"
        );
        let cancel = Frame::build(FrameType::Cancel, control_flags(), 0, 0, 4, Vec::new()).unwrap();
        assert!(
            handler
                .handle_control_frame(&ctx, cancel)
                .await
                .unwrap()
                .is_empty(),
            "the next Cancel is dispatched on the same connection while replies are held"
        );
        assert_eq!(receiver.recv().await.unwrap().header.corr, 0);
        let mut accepted = Vec::new();
        let mut busy = false;
        for _ in 0..5 {
            let reply = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            if reply.header.corr == 5 {
                assert_eq!(
                    serde_json::from_slice::<ErrorBody>(&reply.body)
                        .unwrap()
                        .code,
                    "busy"
                );
                busy = true;
            } else {
                accepted.push(reply.header.corr);
            }
        }
        assert_eq!(
            accepted,
            vec![1, 2, 3, 4],
            "admitted replies preserve arrival order"
        );
        assert!(busy);
        let goodbye =
            Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 6, Vec::new()).unwrap();
        handler.handle_control_frame(&ctx, goodbye).await.unwrap();
        assert!(
            !queues.cancel(ctx.connection_id, 4),
            "Goodbye cleanup removes queued cancellation state"
        );
    }

    #[tokio::test]
    async fn blocked_reply_expires_and_releases_its_declaration_permit() {
        let handler = ControlHandler::new(Arc::new(Registry::default()));
        let queues = Queues::default();
        let (sender, _receiver) = mpsc::channel(1);
        let ctx = RouteCtx {
            connection_id: ConnectionId::new(901),
            egress: FrameSink::new(sender),
        };
        ctx.egress.send(frame(0)).await.unwrap();
        queues
            .enqueue(&handler, &ctx, frame(1), Declaration::default())
            .await
            .unwrap();
        let queue = Arc::clone(queues.0.lock().unwrap().get(&ctx.connection_id).unwrap());
        tokio::time::timeout(Duration::from_secs(2), async {
            while queue.capacity.available_permits() != IN_FLIGHT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a stalled writer must not retain admission forever");
        assert!(queue.fences.lock().unwrap().is_empty());
    }
}
