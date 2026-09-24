//! The spawn-stream consumer: ck-bus follows the supervisor's spawn events and ends the
//! credential of every process that exits.
//!
//! Facts come from the daemon (`source`): `supervisor.spawn_snapshot`, the live processes
//! with their spawn generations and the cursor they were observed at, and
//! `supervisor.spawn_subscribe`, every spawn and exit after a cursor. Generations are the
//! supervisor's; ck-bus never counts them.
//!
//! - An `exited` event revokes the module's census credential when the entry is at that
//!   generation or an earlier one (every exit, whatever its cause: the event carries no
//!   reason on purpose). An entry at a later generation belongs to a respawn and is left
//!   alone. A `spawned` event does nothing: a live generation with no census entry is
//!   issued lazily, on the child's first `ckbus.credential`, because ck-bus has no channel
//!   to push a credential.
//! - Reconciliation lists the census, then takes a snapshot, then revokes every entry
//!   whose (module, generation) the snapshot does not list. The census is read first so
//!   that an entry written after the listing (issuance only writes for a generation that
//!   is live) is never judged against an older snapshot, and each revocation re-checks
//!   that the entry is still at the generation that was listed. It runs at start, after
//!   every gap in the stream, and every `RECONCILE_PERIOD`.
//! - The last processed cursor is kept in `spawn_cursor.json` (`cursor`). At start the
//!   consumer reconciles and then resumes from that cursor, so exits during its own
//!   downtime are replayed. A missing or damaged file resumes from the snapshot instead.
//! - A cursor the daemon refuses (another daemon incarnation, or older than its ring)
//!   means events were missed that can never be replayed: the consumer reconciles
//!   against a fresh snapshot and subscribes from that snapshot's cursor. A stream the
//!   daemon drops for lagging is also a gap: the consumer reconciles, then resubscribes
//!   from the last cursor it received (and falls back to the snapshot if that cursor is
//!   refused in turn). Any other end of the stream resubscribes the same way.
//!
//! Revocation itself belongs to the revocation area (`census` drives its `Revoker`);
//! a revocation that defers keeps its durable record and the area retries it. ck-bus's
//! own module is skipped: its previous box users are revoked by bootstrap, by name.
//!
//! Absence is neutral: a census or snapshot read that fails concludes nothing and is
//! retried after `retry`.

pub mod census;
pub mod cursor;
pub mod source;

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::{json, Value};
use subc_client_rs::consumer::{SpawnCursor, SpawnEvent, SpawnEventKind, SpawnSnapshot};
use tokio::{sync::watch, time::Instant};

use crate::{bootstrap::Ready, revocation::Revoker};
use cursor::{CursorRead, CursorStore};

/// How often reconciliation runs while the stream is healthy.
pub const RECONCILE_PERIOD: Duration = Duration::from_secs(60);

/// How a followed stream ended, other than cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEnd {
    /// The daemon refused the cursor (`spawn_cursor_incarnation_mismatch` or
    /// `spawn_cursor_too_old`): events after it cannot be replayed.
    CursorRefused { code: String },
    /// The daemon dropped the subscriber for falling behind (`spawn_subscriber_lagged`),
    /// after every event it had queued.
    Lagged,
    /// Anything else: connection loss, local backpressure, an undecodable event.
    Failed(String),
}

/// The daemon's spawn facts.
#[async_trait]
pub trait SpawnSource: Send + Sync {
    async fn snapshot(&self) -> Result<SpawnSnapshot, String>;
    /// Events strictly after `since`. A refusal may come back here or as the feed's
    /// first item; both are handled the same way.
    async fn subscribe(&self, since: SpawnCursor) -> Result<Box<dyn SpawnFeed>, FeedEnd>;
}

/// One open subscription.
#[async_trait]
pub trait SpawnFeed: Send {
    /// The next event, `Ok(None)` at a clean end.
    async fn next(&mut self) -> Result<Option<SpawnEvent>, FeedEnd>;
}

/// One census entry: the module it is keyed by and the generation its value names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CensusEntry {
    pub module_id: String,
    pub spawn_generation: u64,
}

/// Which census entries a revocation may take, compared by generation only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fence {
    /// The process at this generation exited: an entry at it or an earlier generation
    /// is dead; a later one belongs to a respawn.
    Exited(u64),
    /// Reconciliation listed the entry at exactly this generation before a snapshot
    /// that lists no process at it; an entry rewritten since is judged again later.
    NotLive(u64),
}

impl Fence {
    pub fn admits(self, entry_generation: u64) -> bool {
        match self {
            Self::Exited(exited) => entry_generation <= exited,
            Self::NotLive(listed) => entry_generation == listed,
        }
    }
}

/// What one revocation request did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeOutcome {
    NoEntry,
    /// The entry names a generation the fence does not admit.
    Fenced {
        entry_generation: u64,
    },
    Revoked {
        entry_generation: u64,
    },
    /// Recorded durably; a step deferred and the revocation area retries it.
    Deferred {
        entry_generation: u64,
        reason: String,
    },
}

/// The census as the consumer uses it.
#[async_trait]
pub trait Census: Send + Sync {
    /// Every entry now. An `Err` is an unreadable census, never an empty one.
    async fn entries(&self) -> Result<Vec<CensusEntry>, String>;
    /// Revokes the module's current entry when `fence` admits its generation. An `Err`
    /// means the entry could not be read, so nothing was concluded.
    async fn revoke(&self, module_id: &str, fence: Fence) -> Result<RevokeOutcome, String>;
}

/// What the consumer has observed, for its log and its tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// `ring_bound` from the last snapshot reply.
    pub ring_bound: Option<u64>,
    /// The last cursor recorded.
    pub cursor: Option<SpawnCursor>,
    pub reconciliations: u64,
}

/// Why the consumer is about to subscribe again.
enum Gap {
    /// Events were missed: resubscribe from a fresh snapshot.
    Missed,
    /// Resume from this cursor (after reconciling).
    Resume(SpawnCursor),
}

pub struct SpawnConsumer {
    source: Arc<dyn SpawnSource>,
    census: Arc<dyn Census>,
    cursors: CursorStore,
    own_module_id: String,
    reconcile_period: Duration,
    retry: Duration,
    status: Arc<Mutex<Status>>,
}

impl SpawnConsumer {
    pub fn new(
        source: Arc<dyn SpawnSource>,
        census: Arc<dyn Census>,
        store_root: &Path,
        own_module_id: String,
        reconcile_period: Duration,
        retry: Duration,
    ) -> Self {
        let cursors = CursorStore::new(store_root);
        cursors.remove_stale_tmp();
        Self {
            source,
            census,
            cursors,
            own_module_id,
            reconcile_period,
            retry,
            status: Arc::new(Mutex::new(Status::default())),
        }
    }

    pub fn status(&self) -> Arc<Mutex<Status>> {
        self.status.clone()
    }

    fn update_status(&self, update: impl FnOnce(&mut Status)) {
        update(
            &mut self
                .status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
    }

    /// Runs until the task is dropped.
    pub async fn run(self) {
        let mut resume = match self.cursors.read() {
            CursorRead::Present(cursor) => Some(cursor),
            CursorRead::Absent => None,
            CursorRead::Damaged { reason } => {
                log_event(
                    "ckbus.spawn.cursor_damaged",
                    json!({
                        "path": self.cursors.path().display().to_string(),
                        "reason": reason,
                        "action": "read as absent: reconciling against a fresh snapshot",
                    }),
                );
                None
            }
        };
        let mut reason = "start";
        loop {
            let snapshot = match self.reconcile(reason).await {
                Ok((snapshot, _)) => snapshot,
                Err(error) => {
                    log_event(
                        "ckbus.spawn.reconcile_deferred",
                        json!({ "reason": reason, "error": error }),
                    );
                    tokio::time::sleep(self.retry).await;
                    continue;
                }
            };
            let since = match resume.take() {
                Some(cursor) => cursor,
                None => {
                    self.record_cursor(&snapshot.cursor);
                    snapshot.cursor
                }
            };
            log_event(
                "ckbus.spawn.subscribing",
                json!({ "since": since, "reason": reason }),
            );
            match self.follow(since).await {
                (Gap::Missed, why) => {
                    reason = why;
                }
                (Gap::Resume(from), why) => {
                    resume = Some(from);
                    reason = why;
                    if why == "stream-failed" {
                        // A daemon that is going away fails every call at once; waiting
                        // keeps the loop from spinning against it.
                        tokio::time::sleep(self.retry).await;
                    }
                }
            }
        }
    }

    /// Follows one subscription until it ends; returns what to do next and why.
    async fn follow(&self, since: SpawnCursor) -> (Gap, &'static str) {
        let mut feed = match self.source.subscribe(since.clone()).await {
            Ok(feed) => feed,
            Err(end) => return self.gap(end, since),
        };
        let mut last = since;
        let mut next_reconcile = Instant::now() + self.reconcile_period;
        loop {
            tokio::select! {
                item = feed.next() => match item {
                    Ok(Some(event)) => {
                        if !self.handle(&event).await {
                            next_reconcile = next_reconcile.min(Instant::now() + self.retry);
                        }
                        last = event.cursor.clone();
                        self.record_cursor(&last);
                    }
                    Ok(None) => {
                        log_event("ckbus.spawn.stream_ended", json!({ "last": last }));
                        return (Gap::Resume(last), "stream-ended");
                    }
                    Err(end) => return self.gap(end, last),
                },
                _ = tokio::time::sleep_until(next_reconcile) => {
                    let complete = match self.reconcile("periodic").await {
                        Ok((_, complete)) => complete,
                        Err(error) => {
                            log_event(
                                "ckbus.spawn.reconcile_deferred",
                                json!({ "reason": "periodic", "error": error }),
                            );
                            false
                        }
                    };
                    next_reconcile = Instant::now()
                        + if complete { self.reconcile_period } else { self.retry };
                }
            }
        }
    }

    fn gap(&self, end: FeedEnd, last: SpawnCursor) -> (Gap, &'static str) {
        match end {
            FeedEnd::CursorRefused { code } => {
                log_event(
                    "ckbus.spawn.cursor_refused",
                    json!({
                        "code": code,
                        "since": last,
                        "action": "events were missed: reconciling against a fresh snapshot \
                                   and subscribing from its cursor",
                    }),
                );
                (Gap::Missed, "cursor-refused")
            }
            FeedEnd::Lagged => {
                log_event(
                    "ckbus.spawn.lagged",
                    json!({
                        "last": last,
                        "action": "reconciling, then resubscribing from the last cursor \
                                   received",
                    }),
                );
                (Gap::Resume(last), "lagged")
            }
            FeedEnd::Failed(error) => {
                log_event(
                    "ckbus.spawn.stream_failed",
                    json!({ "last": last, "error": error }),
                );
                (Gap::Resume(last), "stream-failed")
            }
        }
    }

    /// Handles one event. `false` when a census read failed, so reconciliation should
    /// come sooner than its period.
    async fn handle(&self, event: &SpawnEvent) -> bool {
        let fields = |action: Value| {
            json!({
                "kind": event.kind,
                "module_id": event.module_id,
                "spawn_generation": event.spawn_generation,
                "cursor": event.cursor,
                "action": action,
            })
        };
        if event.module_id == self.own_module_id {
            log_event(
                "ckbus.spawn.event",
                fields(json!("own module: its box users are revoked by bootstrap")),
            );
            return true;
        }
        if event.kind == SpawnEventKind::Spawned {
            log_event(
                "ckbus.spawn.event",
                fields(json!("none: issued on the first ckbus.credential")),
            );
            return true;
        }
        match self
            .census
            .revoke(&event.module_id, Fence::Exited(event.spawn_generation))
            .await
        {
            Ok(outcome) => {
                log_event("ckbus.spawn.event", fields(revoked_json(&outcome)));
                true
            }
            Err(error) => {
                log_event(
                    "ckbus.spawn.event",
                    fields(json!({ "deferred": error, "retry": "reconciliation" })),
                );
                false
            }
        }
    }

    /// Lists the census, then snapshots, then revokes every entry the snapshot does not
    /// list as live. Returns the snapshot, and whether every entry was judged.
    async fn reconcile(&self, reason: &str) -> Result<(SpawnSnapshot, bool), String> {
        let entries = self
            .census
            .entries()
            .await
            .map_err(|error| format!("census unreadable: {error}"))?;
        let snapshot = self.source.snapshot().await?;
        let mut live: HashMap<&str, BTreeSet<u64>> = HashMap::new();
        for spawn in &snapshot.live {
            live.entry(spawn.module_id.as_str())
                .or_default()
                .insert(spawn.spawn_generation);
        }
        let mut complete = true;
        let mut judged = Vec::new();
        for entry in &entries {
            if entry.module_id == self.own_module_id
                || live
                    .get(entry.module_id.as_str())
                    .is_some_and(|generations| generations.contains(&entry.spawn_generation))
            {
                continue;
            }
            let outcome = match self
                .census
                .revoke(&entry.module_id, Fence::NotLive(entry.spawn_generation))
                .await
            {
                Ok(outcome) => revoked_json(&outcome),
                Err(error) => {
                    complete = false;
                    json!({ "deferred": error })
                }
            };
            judged.push(json!({
                "module_id": entry.module_id,
                "spawn_generation": entry.spawn_generation,
                "outcome": outcome,
            }));
        }
        self.update_status(|status| {
            status.ring_bound = Some(snapshot.ring_bound);
            status.reconciliations += 1;
        });
        log_event(
            "ckbus.spawn.reconciled",
            json!({
                "reason": reason,
                "cursor": snapshot.cursor,
                "ring_bound": snapshot.ring_bound,
                "live": snapshot.live.len(),
                "census_entries": entries.len(),
                "not_live": judged,
                "complete": complete,
            }),
        );
        Ok((snapshot, complete))
    }

    fn record_cursor(&self, cursor: &SpawnCursor) {
        if let Err(error) = self.cursors.write(cursor) {
            // A restart then replays from an older cursor, which only repeats revocations
            // that are no-ops by then.
            log_event(
                "ckbus.spawn.cursor_unwritable",
                json!({
                    "path": self.cursors.path().display().to_string(),
                    "error": error.to_string(),
                }),
            );
        }
        self.update_status(|status| status.cursor = Some(cursor.clone()));
    }
}

fn revoked_json(outcome: &RevokeOutcome) -> Value {
    match outcome {
        RevokeOutcome::NoEntry => json!("no census entry"),
        RevokeOutcome::Fenced { entry_generation } => {
            json!({ "fenced": { "entry_generation": entry_generation } })
        }
        RevokeOutcome::Revoked { entry_generation } => {
            json!({ "revoked": { "entry_generation": entry_generation } })
        }
        RevokeOutcome::Deferred {
            entry_generation,
            reason,
        } => json!({ "revocation_deferred": {
            "entry_generation": entry_generation,
            "reason": reason,
        }}),
    }
}

/// The one wiring call: the consumer over ck-bus's daemon connection file and the
/// revocation area's `Revoker`, started as a task. `retry` is the sentinel period, the
/// retry interval of every deferral.
pub fn wire(
    connection_file: PathBuf,
    revoker: Arc<Revoker>,
    store_root: &Path,
    ready: watch::Receiver<Option<Arc<Ready>>>,
    own_module_id: String,
    retry: Duration,
) {
    let consumer = SpawnConsumer::new(
        Arc::new(source::DaemonSource::new(connection_file)),
        Arc::new(census::RevokerCensus::new(revoker, ready)),
        store_root,
        own_module_id,
        RECONCILE_PERIOD,
        retry,
    );
    tokio::spawn(consumer.run());
}

/// One structured stderr line, in the same shape as bootstrap's.
pub(crate) fn log_event(event: &str, fields: Value) {
    let at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    let mut line = json!({ "event": event, "at_ms": at_ms });
    if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), fields) {
        line.extend(fields);
    }
    eprintln!("{line}");
}
