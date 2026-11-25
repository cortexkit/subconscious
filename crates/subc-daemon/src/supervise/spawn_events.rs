use super::*;

/// Maximum number of supervised process spawn/exit facts retained per daemon incarnation.
pub const SPAWN_EVENT_RING_CAPACITY: usize = 4096;
pub(super) const SPAWN_SUBSCRIBER_BUFFER: usize = SPAWN_EVENT_RING_CAPACITY + 1;
/// Terminal Error code for a `supervisor.spawn_subscribe` stream the daemon
/// dropped because the subscriber stopped draining its frames. The detail's
/// `first_undelivered_cursor` names the first event it did not receive; the
/// client resubscribes from the last cursor it did receive.
pub(super) const SPAWN_SUBSCRIBER_LAGGED_CODE: &str = "spawn_subscriber_lagged";

type SpawnSubscriberKey = (ConnectionId, u64);

#[derive(Debug)]
struct SpawnSubscriber {
    version: u8,
    frames: mpsc::Sender<Frame>,
    /// Tells this subscriber's forwarder that it was dropped for lagging, and
    /// from which event. The full frame channel cannot carry that news, so it
    /// travels beside it; see `SpawnEventFeed::subscribe`.
    lagged: Option<oneshot::Sender<SpawnCursor>>,
}

#[derive(Debug)]
pub(super) struct SpawnEventState {
    pub(super) daemon_incarnation: String,
    pub(super) seq: u64,
    capacity: usize,
    pub(super) live: HashMap<String, LiveSpawn>,
    pub(super) generations: HashMap<String, u64>,
    pub(super) events: VecDeque<SpawnEvent>,
    subscribers: HashMap<SpawnSubscriberKey, SpawnSubscriber>,
    pub(super) process_changes: HashMap<String, tokio::sync::watch::Sender<u64>>,
}

impl Default for SpawnEventState {
    fn default() -> Self {
        Self {
            daemon_incarnation: "unconfigured".to_string(),
            seq: 0,
            capacity: SPAWN_EVENT_RING_CAPACITY,
            live: HashMap::new(),
            generations: HashMap::new(),
            events: VecDeque::new(),
            subscribers: HashMap::new(),
            process_changes: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct SpawnEventFeed(
    pub(super) Arc<Mutex<SpawnEventState>>,
    #[cfg(test)] pub(super) Arc<Mutex<Option<std::net::TcpStream>>>,
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpawnSubscribeRefusal {
    ForeignIncarnation { current: String },
    TooOld { oldest: SpawnCursor },
    Frame(String),
}

impl SpawnEventFeed {
    pub(super) fn configure_incarnation(&self, daemon_incarnation: String) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.daemon_incarnation = daemon_incarnation;
        state.seq = 0;
        state.live.clear();
        state.generations.clear();
        state.events.clear();
        state.subscribers.clear();
        for changed in state.process_changes.values() {
            changed.send_replace(0);
        }
    }

    fn cursor(state: &SpawnEventState) -> SpawnCursor {
        SpawnCursor {
            daemon_incarnation: state.daemon_incarnation.clone(),
            seq: state.seq,
        }
    }

    pub(super) fn snapshot(&self) -> SpawnSnapshot {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let mut live = state.live.values().cloned().collect::<Vec<_>>();
        live.sort_by(|left, right| left.module_id.cmp(&right.module_id));
        SpawnSnapshot {
            cursor: Self::cursor(&state),
            ring_bound: state.capacity as u64,
            live,
        }
    }

    fn allocate_generation_locked(state: &mut SpawnEventState, module_id: &str) -> u64 {
        let generation = state
            .generations
            .get(module_id)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .expect("spawn generation exhausted");
        state.generations.insert(module_id.to_string(), generation);
        generation
    }

    /// Allocate the spawn generation of a launch that is about to start, with
    /// the daemon incarnation it belongs to. The number names the launch from
    /// this moment, before its process runs; a launch that never runs keeps
    /// its number, so the generations a subscriber sees may skip.
    pub(super) fn allocate_generation(&self, module_id: &str) -> (String, u64) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let generation = Self::allocate_generation_locked(&mut state, module_id);
        (state.daemon_incarnation.clone(), generation)
    }

    /// Reserve the generation of the verified child before any later spawn.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn reserve_adopted_generation(&self, module: &str, generation: u64) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let latest = state.generations.entry(module.to_owned()).or_default();
        *latest = (*latest).max(generation);
    }

    /// Publish a running process. `allocated` is the generation its launch
    /// was given before it started; a launch without one (a process with no
    /// registration channel) is numbered here.
    pub(super) fn emit_spawned(
        &self,
        module_id: &str,
        allocated: Option<u64>,
        pid: u32,
        spawned_at_ms: u64,
    ) -> u64 {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let generation = match allocated {
            Some(generation) => generation,
            None => Self::allocate_generation_locked(&mut state, module_id),
        };
        let live = LiveSpawn {
            module_id: module_id.to_string(),
            spawn_generation: generation,
            pid,
            spawned_at_ms,
        };
        state.live.insert(module_id.to_string(), live);
        Self::emit_locked(
            &mut state,
            SpawnEventKind::Spawned,
            module_id.to_string(),
            generation,
            pid,
            None,
            None,
        );
        generation
    }

    pub(super) fn emit_exited(
        &self,
        module_id: &str,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(live) = state.live.remove(module_id) else {
            warn!(
                module_id,
                "terminal record had no live spawn event identity"
            );
            return;
        };
        Self::emit_locked(
            &mut state,
            SpawnEventKind::Exited,
            module_id.to_string(),
            live.spawn_generation,
            live.pid,
            exit_code,
            exit_signal,
        );
    }

    /// Report the exit of a process that a swap has already replaced.
    ///
    /// `emit_exited` removes the module's live entry, which after a swap's
    /// cutover describes the promoted candidate, not the old process now
    /// exiting. This emits the old generation's exit and leaves the live entry
    /// alone unless it still names that generation.
    pub(super) fn emit_superseded_exited(
        &self,
        module_id: &str,
        spawn_generation: u64,
        pid: u32,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .live
            .get(module_id)
            .is_some_and(|live| live.spawn_generation == spawn_generation)
        {
            state.live.remove(module_id);
        }
        Self::emit_locked(
            &mut state,
            SpawnEventKind::Exited,
            module_id.to_string(),
            spawn_generation,
            pid,
            exit_code,
            exit_signal,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_locked(
        state: &mut SpawnEventState,
        kind: SpawnEventKind,
        module_id: String,
        spawn_generation: u64,
        pid: u32,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    ) {
        state.seq = state
            .seq
            .checked_add(1)
            .expect("spawn event sequence exhausted");
        let event = SpawnEvent {
            cursor: Self::cursor(state),
            kind,
            module_id,
            spawn_generation,
            pid,
            exit_code,
            exit_signal,
        };
        state
            .process_changes
            .retain(|_, changed| changed.receiver_count() > 0);
        if let Some(changed) = state.process_changes.get(&event.module_id) {
            changed.send_replace(state.seq);
        }
        state.events.push_back(event.clone());
        while state.events.len() > state.capacity {
            state.events.pop_front();
        }
        let body = match serde_json::to_vec(&event) {
            Ok(body) => body,
            Err(error) => {
                error!(%error, "failed to serialize supervisor spawn event");
                return;
            }
        };
        state.subscribers.retain(|(connection_id, corr), subscriber| {
            let frame = Frame::build_with_version(
                subscriber.version,
                FrameType::StreamData,
                control_flags(),
                0,
                0,
                *corr,
                body.clone(),
            );
            match frame {
                Ok(frame) => {
                    if subscriber.frames.try_send(frame).is_ok() {
                        true
                    } else {
                        warn!(connection_id = connection_id.get(), corr, "dropping lagged supervisor spawn subscriber");
                        if let Some(lagged) = subscriber.lagged.take() {
                            let _ = lagged.send(event.cursor.clone());
                        }
                        false
                    }
                }
                Err(error) => {
                    warn!(connection_id = connection_id.get(), corr, %error, "dropping supervisor spawn subscriber after frame build failure");
                    false
                }
            }
        });
    }

    pub(super) fn subscribe(
        &self,
        connection_id: ConnectionId,
        corr: u64,
        version: u8,
        since: Option<SpawnCursor>,
        sink: FrameSink,
    ) -> Result<(), SpawnSubscribeRefusal> {
        let (frames, mut receiver) = mpsc::channel(SPAWN_SUBSCRIBER_BUFFER);
        let (lagged, mut lagged_rx) = oneshot::channel::<SpawnCursor>();
        {
            let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let replay = if let Some(since) = since {
                if since.daemon_incarnation != state.daemon_incarnation {
                    return Err(SpawnSubscribeRefusal::ForeignIncarnation {
                        current: state.daemon_incarnation.clone(),
                    });
                }
                if let Some(oldest) = state.events.front().map(|event| event.cursor.clone()) {
                    if since.seq < oldest.seq.saturating_sub(1) {
                        return Err(SpawnSubscribeRefusal::TooOld { oldest });
                    }
                }
                state
                    .events
                    .iter()
                    .filter(|event| event.cursor.seq > since.seq)
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            for event in replay {
                let body = serde_json::to_vec(&event)
                    .map_err(|error| SpawnSubscribeRefusal::Frame(error.to_string()))?;
                let frame = Frame::build_with_version(
                    version,
                    FrameType::StreamData,
                    control_flags(),
                    0,
                    0,
                    corr,
                    body,
                )
                .map_err(|error| SpawnSubscribeRefusal::Frame(error.to_string()))?;
                frames
                    .try_send(frame)
                    .map_err(|error| SpawnSubscribeRefusal::Frame(error.to_string()))?;
            }
            state.subscribers.insert(
                (connection_id, corr),
                SpawnSubscriber {
                    version,
                    frames,
                    lagged: Some(lagged),
                },
            );
        }
        // The lagged terminal is sent here, by the forwarder, rather than by
        // the emitter: at the moment of the drop the subscriber's own channel
        // is full, and writing to the connection sink directly from the emitter
        // would put the Error AHEAD of the events still queued in that channel
        // (and the emitter holds the feed lock, so it cannot await the sink).
        // Dropping the subscriber drops the only sender, so `recv` drains every
        // queued event and then returns `None`; only then is the Error sent, so
        // the client sees each event it can keep, then the reason it was cut.
        // Cancel and connection removal drop the oneshot unsent, so they end
        // the stream with no Error.
        tokio::spawn(async move {
            while let Some(frame) = receiver.recv().await {
                if sink.send(frame).await.is_err() {
                    return;
                }
            }
            let Ok(first_undelivered) = lagged_rx.try_recv() else {
                return;
            };
            match spawn_subscriber_lagged_frame(version, corr, first_undelivered) {
                Ok(frame) => {
                    let _ = sink.send(frame).await;
                }
                Err(error) => {
                    error!(%error, corr, "failed to build lagged spawn subscriber terminal frame");
                }
            }
        });
        Ok(())
    }

    fn cancel(&self, connection_id: ConnectionId, corr: u64) -> bool {
        let Some(subscriber) = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribers
            .remove(&(connection_id, corr))
        else {
            return false;
        };
        if let Ok(frame) = Frame::build_with_version(
            subscriber.version,
            FrameType::StreamEnd,
            control_flags(),
            0,
            0,
            corr,
            Vec::new(),
        ) {
            tokio::spawn(async move {
                let _ = subscriber.frames.send(frame).await;
            });
        }
        true
    }

    fn remove_connection(&self, connection_id: ConnectionId) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribers
            .retain(|(subscriber_connection, _), _| *subscriber_connection != connection_id);
    }

    #[cfg(any(test, feature = "test-support"))]
    fn set_capacity(&self, capacity: usize) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).capacity = capacity;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn subscriber_count(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribers
            .len()
    }
}

/// The terminal Error a lagged spawn subscriber receives after its queued events.
fn spawn_subscriber_lagged_frame(
    version: u8,
    corr: u64,
    first_undelivered: SpawnCursor,
) -> Result<Frame, String> {
    // The daemon authors this terminal Error, so it carries the daemon-origin bit.
    let body = serde_json::to_vec(&subc_protocol::ErrorBody {
        code: SPAWN_SUBSCRIBER_LAGGED_CODE.to_string(),
        message: "spawn subscriber fell behind and was dropped; resubscribe from the last cursor received"
            .to_string(),
        detail: Some(serde_json::json!({
            "first_undelivered_cursor": first_undelivered
        })),
    })
    .map_err(|error| error.to_string())?;
    Frame::build_with_version(
        version,
        FrameType::Error,
        crate::router::daemon_error_flags(),
        0,
        0,
        corr,
        body,
    )
    .map_err(|error| error.to_string())
}

impl SupervisorHandle {
    pub(crate) fn spawn_snapshot(&self) -> SpawnSnapshot {
        self.spawn_events.snapshot()
    }

    /// Subscribe before reading the bound process. A coalesced wake is enough:
    /// readers compare its exact generation and PID, not the last event kind.
    pub(crate) fn subscribe_process_changes(
        &self,
        module: &str,
    ) -> tokio::sync::watch::Receiver<u64> {
        let mut state = self
            .spawn_events
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        state
            .process_changes
            .retain(|_, changed| changed.receiver_count() > 0);
        let seq = state.seq;
        state
            .process_changes
            .entry(module.into())
            .or_insert_with(|| tokio::sync::watch::channel(seq).0)
            .subscribe()
    }

    pub(crate) fn current_spawn(&self, module: &str) -> Option<LiveSpawn> {
        self.spawn_events
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .live
            .get(module)
            .cloned()
    }

    pub(crate) fn subscribe_spawns(
        &self,
        connection_id: ConnectionId,
        corr: u64,
        version: u8,
        since: Option<SpawnCursor>,
        sink: FrameSink,
    ) -> Result<(), SpawnSubscribeRefusal> {
        self.spawn_events
            .subscribe(connection_id, corr, version, since, sink)
    }

    pub(crate) fn cancel_spawn_subscription(&self, connection_id: ConnectionId, corr: u64) -> bool {
        self.spawn_events.cancel(connection_id, corr)
    }

    pub(crate) fn remove_spawn_subscribers(&self, connection_id: ConnectionId) {
        self.spawn_events.remove_connection(connection_id);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_spawn_event_capacity_for_test(&self, capacity: usize) {
        assert!(capacity > 0, "spawn event capacity must be non-zero");
        self.spawn_events.set_capacity(capacity);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_subscriber_count_for_test(&self) -> usize {
        self.spawn_events.subscriber_count()
    }
}
