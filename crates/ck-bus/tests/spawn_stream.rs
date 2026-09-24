//! Ladder row "Spawn-stream consumer" (slice 7 of `docs/specs/ck-bus-module.md`).
//!
//! Serving side: none. No arm opens a Claustrum or Callosum route. The daemon arms run
//! the acceptance daemon (the shape stubs registered, never reached) and drive ck-bus's
//! spawn consumer in this process against it, with a recording census in place of the
//! box account: the row is about what the consumer asks for and when, and it issues
//! and revokes nothing itself. Revocation through the real census is the reconciliation
//! row's (`spawn_reconcile.rs`, harness-signer).
//!
//! Arms:
//! - Both ops appear in `catalog.list`'s `subc_ops`; the observed list is printed.
//! - Against the real daemon: every exit (a supervised restart, and a `SIGKILL` the
//!   supervisor did not ask for) asks the census to revoke at exactly the exited
//!   generation; a spawn asks for nothing. The recorded cursor is the last event's, and
//!   `ring_bound` is the daemon's reply. A consumer stopped and started again over the
//!   same store resumes from that cursor and sees the exit that happened while it was
//!   down.
//! - Against the real daemon: a cursor from another daemon incarnation is refused with
//!   `spawn_cursor_incarnation_mismatch` and exactly the detail key
//!   `current_daemon_incarnation`; a consumer holding such a cursor reconciles against a
//!   fresh snapshot and subscribes from that snapshot's cursor.
//! - Scripted (the daemon's ring holds 4096 events and a subscriber is dropped only
//!   past 4097 queued, neither drivable here in bounded time): a cursor older than the
//!   ring (`spawn_cursor_too_old`) falls back to a snapshot and reconciles; a lagged
//!   stream reconciles and resubscribes from its last cursor, and when that cursor is
//!   refused in turn it falls back to the snapshot. The daemon's own tests assert those
//!   two frames byte-exact; `subc-client-rs`'s unit tests assert the client surfaces
//!   them with their codes and detail, and `classify` below maps them.
//!
//! Mutation controls, each a gap the consumer must converge across: a foreign
//! incarnation, a cursor older than the ring, a lagged stream, a lagged stream whose
//! last cursor is then too old, and (in the reconciliation row) a damaged
//! `spawn_cursor.json` and a respawn while ck-bus is down.

#[allow(dead_code)]
#[path = "../src/bootstrap/mod.rs"]
mod bootstrap;
#[allow(dead_code)]
#[path = "../src/credentials/mod.rs"]
mod credentials;
#[allow(dead_code)]
#[path = "../src/grants/mod.rs"]
mod grants;
#[allow(dead_code)]
mod harness;
#[allow(dead_code)]
#[path = "../src/issuance/mod.rs"]
mod issuance;
#[allow(dead_code)]
#[path = "../src/revocation/mod.rs"]
mod revocation;
#[allow(dead_code)]
#[path = "../src/runtime/seams.rs"]
mod runtime;
#[allow(dead_code)]
#[path = "../src/spawn_consumer/mod.rs"]
mod spawn_consumer;
#[allow(dead_code)]
#[cfg(unix)]
#[path = "support/mod.rs"]
mod support;

use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use harness::{
    control,
    daemon::AcceptanceRun,
    report::{Row, RowReport, ServedBy},
};
use spawn_consumer::{
    Census, CensusEntry, FeedEnd, Fence, RevokeOutcome, SpawnConsumer, SpawnFeed, SpawnSource,
    Status,
};
use subc_client_rs::consumer::{
    CallError, SpawnCursor, SpawnEvent, SpawnEventKind, SpawnSnapshot, SpawnStreamError,
};
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::ErrorBody;

const OWN: &str = "ckbus";
const WAIT: Duration = Duration::from_secs(20);

fn passed() {
    RowReport::passed(Row::SpawnStream)
        .served_by(ServedBy::None)
        .emit(&BTreeSet::new());
}

fn cursor(incarnation: &str, seq: u64) -> SpawnCursor {
    SpawnCursor {
        daemon_incarnation: incarnation.to_string(),
        seq,
    }
}

fn store_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ckbus-spawn-stream-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < deadline, "{what} within {WAIT:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A census that records every request and answers from a list the arm sets.
#[derive(Default)]
struct RecordingCensus {
    entries: Mutex<Vec<CensusEntry>>,
    listings: Mutex<usize>,
    revokes: Mutex<Vec<(String, Fence)>>,
}

impl RecordingCensus {
    fn with(entries: Vec<CensusEntry>) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(entries),
            ..Self::default()
        })
    }

    fn revokes(&self) -> Vec<(String, Fence)> {
        self.revokes.lock().unwrap().clone()
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn listings(&self) -> usize {
        *self.listings.lock().unwrap()
    }
}

#[async_trait]
impl Census for RecordingCensus {
    async fn entries(&self) -> Result<Vec<CensusEntry>, String> {
        *self.listings.lock().unwrap() += 1;
        Ok(self.entries.lock().unwrap().clone())
    }

    async fn revoke(&self, module_id: &str, fence: Fence) -> Result<RevokeOutcome, String> {
        self.revokes
            .lock()
            .unwrap()
            .push((module_id.to_string(), fence));
        let mut entries = self.entries.lock().unwrap();
        let Some(index) = entries
            .iter()
            .position(|entry| entry.module_id == module_id)
        else {
            return Ok(RevokeOutcome::NoEntry);
        };
        let entry_generation = entries[index].spawn_generation;
        if !fence.admits(entry_generation) {
            return Ok(RevokeOutcome::Fenced { entry_generation });
        }
        entries.remove(index);
        Ok(RevokeOutcome::Revoked { entry_generation })
    }
}

/// One request the consumer made of its source.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    /// The snapshot's own cursor, as answered.
    Snapshot(SpawnCursor),
    Subscribe(SpawnCursor),
}

/// Records every request and forwards it to `inner`.
struct Recording {
    inner: Arc<dyn SpawnSource>,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Recording {
    fn new(inner: Arc<dyn SpawnSource>) -> (Arc<Self>, Arc<Mutex<Vec<Call>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                inner,
                calls: calls.clone(),
            }),
            calls,
        )
    }
}

#[async_trait]
impl SpawnSource for Recording {
    async fn snapshot(&self) -> Result<SpawnSnapshot, String> {
        let snapshot = self.inner.snapshot().await?;
        self.calls
            .lock()
            .unwrap()
            .push(Call::Snapshot(snapshot.cursor.clone()));
        Ok(snapshot)
    }

    async fn subscribe(&self, since: SpawnCursor) -> Result<Box<dyn SpawnFeed>, FeedEnd> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Subscribe(since.clone()));
        self.inner.subscribe(since).await
    }
}

/// A scripted daemon: snapshots answered in order (the last one repeats), and one
/// scripted feed per subscription, which stays open once its items run out.
struct Scripted {
    snapshots: Mutex<VecDeque<SpawnSnapshot>>,
    feeds: Mutex<VecDeque<Vec<Result<SpawnEvent, FeedEnd>>>>,
}

impl Scripted {
    fn new(
        snapshots: Vec<SpawnSnapshot>,
        feeds: Vec<Vec<Result<SpawnEvent, FeedEnd>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            snapshots: Mutex::new(snapshots.into()),
            feeds: Mutex::new(feeds.into()),
        })
    }
}

#[async_trait]
impl SpawnSource for Scripted {
    async fn snapshot(&self) -> Result<SpawnSnapshot, String> {
        let mut snapshots = self.snapshots.lock().unwrap();
        if snapshots.len() > 1 {
            Ok(snapshots.pop_front().unwrap())
        } else {
            snapshots.front().cloned().ok_or("no snapshot".to_string())
        }
    }

    async fn subscribe(&self, _since: SpawnCursor) -> Result<Box<dyn SpawnFeed>, FeedEnd> {
        let items = self.feeds.lock().unwrap().pop_front().unwrap_or_default();
        Ok(Box::new(ScriptedFeed(items.into())))
    }
}

struct ScriptedFeed(VecDeque<Result<SpawnEvent, FeedEnd>>);

#[async_trait]
impl SpawnFeed for ScriptedFeed {
    async fn next(&mut self) -> Result<Option<SpawnEvent>, FeedEnd> {
        match self.0.pop_front() {
            Some(item) => item.map(Some),
            None => std::future::pending().await,
        }
    }
}

fn snapshot(at: SpawnCursor, live: &[(&str, u64)]) -> SpawnSnapshot {
    SpawnSnapshot {
        cursor: at,
        ring_bound: 4096,
        live: live
            .iter()
            .map(|(module_id, generation)| subc_control::LiveSpawn {
                module_id: module_id.to_string(),
                spawn_generation: *generation,
                pid: 1,
                spawned_at_ms: 0,
            })
            .collect(),
    }
}

fn exited(at: SpawnCursor, module_id: &str, generation: u64) -> SpawnEvent {
    SpawnEvent {
        cursor: at,
        kind: SpawnEventKind::Exited,
        module_id: module_id.to_string(),
        spawn_generation: generation,
        pid: 1,
        exit_code: Some(0),
        exit_signal: None,
    }
}

/// Starts a consumer over `store`; returns its task and its status.
fn start_consumer(
    source: Arc<dyn SpawnSource>,
    census: Arc<dyn Census>,
    store: &Path,
) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Status>>) {
    let consumer = SpawnConsumer::new(
        source,
        census,
        store,
        OWN.to_string(),
        Duration::from_secs(3600),
        Duration::from_millis(100),
    );
    let status = consumer.status();
    (tokio::spawn(consumer.run()), status)
}

fn calls_of(calls: &Arc<Mutex<Vec<Call>>>) -> Vec<Call> {
    calls.lock().unwrap().clone()
}

fn stored_cursor(store: &Path) -> SpawnCursor {
    serde_json::from_slice(&std::fs::read(store.join("spawn_cursor.json")).unwrap())
        .expect("spawn_cursor.json holds a cursor")
}

#[cfg_attr(not(unix), allow(dead_code))]
async fn daemon_snapshot(connection_file: &Path) -> SpawnSnapshot {
    let response = control::response(
        connection_file,
        ClientControlRequest::SupervisorSpawnSnapshot {},
    )
    .await;
    let ClientControlResponse::SupervisorSpawnSnapshot { snapshot } = response else {
        panic!("supervisor.spawn_snapshot must return its matching response variant");
    };
    snapshot
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_ops_are_advertised_and_the_observed_list_is_recorded() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let run = AcceptanceRun::start(Path::new(env!("CARGO_BIN_EXE_ck-bus"))).await;
    let response = control::response(
        &run.connection_file,
        ClientControlRequest::CatalogList { module_id: None },
    )
    .await;
    let ClientControlResponse::CatalogList { subc_ops, .. } = response else {
        panic!("catalog.list must return its matching response variant");
    };
    eprintln!("observed subc_ops: {subc_ops:?}");
    for op in ["supervisor.spawn_snapshot", "supervisor.spawn_subscribe"] {
        assert!(
            subc_ops.iter().any(|advertised| advertised == op),
            "{op} is not in subc_ops {subc_ops:?}"
        );
    }
    run.shutdown().await;
    passed();
}

#[test]
fn the_daemons_stream_codes_classify_as_the_gaps_they_are() {
    let module_error = |code: &str| {
        SpawnStreamError::Call(CallError::Module(ErrorBody {
            code: code.to_string(),
            message: "sent by the test".to_string(),
            detail: None,
        }))
    };
    for code in ["spawn_cursor_incarnation_mismatch", "spawn_cursor_too_old"] {
        assert_eq!(
            spawn_consumer::source::classify(&module_error(code)),
            FeedEnd::CursorRefused {
                code: code.to_string()
            }
        );
    }
    assert_eq!(
        spawn_consumer::source::classify(&module_error("spawn_subscriber_lagged")),
        FeedEnd::Lagged
    );
    assert!(matches!(
        spawn_consumer::source::classify(&module_error("module_timeout")),
        FeedEnd::Failed(_)
    ));
}

#[cfg(unix)]
async fn restart(connection_file: &Path, module_id: &str) {
    let reply = control::rpc(
        connection_file,
        ClientControlRequest::SupervisorRestart {
            module_id: module_id.to_string(),
            drain_timeout_ms: None,
        },
    )
    .await;
    if let control::ControlReply::Error(error) = reply {
        panic!(
            "supervisor.restart {module_id} refused: {} {}",
            error.code, error.message
        );
    }
}

#[cfg(unix)]
async fn live_generation(connection_file: &Path, module_id: &str) -> Option<u64> {
    daemon_snapshot(connection_file)
        .await
        .live
        .iter()
        .find(|spawn| spawn.module_id == module_id)
        .map(|spawn| spawn.spawn_generation)
}

#[cfg(unix)]
async fn wait_generation(connection_file: &Path, module_id: &str, generation: u64) {
    let deadline = Instant::now() + WAIT;
    while live_generation(connection_file, module_id).await != Some(generation) {
        assert!(
            Instant::now() < deadline,
            "{module_id} never reached generation {generation}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_exit_is_revoked_by_fact_and_a_restarted_consumer_resumes_from_its_cursor() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let run = AcceptanceRun::start(Path::new(env!("CARGO_BIN_EXE_ck-bus"))).await;
    let standin = "standin-none";
    support::enable(&run, standin).await;
    let first = live_generation(&run.connection_file, standin)
        .await
        .expect("the stand-in is live");
    let store = store_dir("resume");

    let census = RecordingCensus::with(vec![]);
    let (source, calls) = Recording::new(Arc::new(spawn_consumer::source::DaemonSource::new(
        run.connection_file.clone(),
    )));
    let (task, status) = start_consumer(source, census.clone(), &store);
    until("the consumer subscribes", || {
        calls_of(&calls)
            .iter()
            .any(|call| matches!(call, Call::Subscribe(_)))
            .then_some(())
    })
    .await;
    let first_snapshot = daemon_snapshot(&run.connection_file).await;
    assert_eq!(
        status.lock().unwrap().ring_bound,
        Some(first_snapshot.ring_bound),
        "ring_bound is read from the daemon's reply"
    );

    // A supervised restart, then a kill nobody asked for: both are exits.
    restart(&run.connection_file, standin).await;
    wait_generation(&run.connection_file, standin, first + 1).await;
    let pid = support::pid(&run, standin).await;
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    wait_generation(&run.connection_file, standin, first + 2).await;
    let expected = vec![
        (standin.to_string(), Fence::Exited(first)),
        (standin.to_string(), Fence::Exited(first + 1)),
    ];
    until("both exits are revoked", || {
        (census.revokes() == expected).then_some(())
    })
    .await;
    // The recorded cursor is the daemon's newest (the respawn after the kill).
    let newest = daemon_snapshot(&run.connection_file).await.cursor;
    until("the cursor reaches the newest event", || {
        (status.lock().unwrap().cursor.as_ref() == Some(&newest)).then_some(())
    })
    .await;
    assert_eq!(stored_cursor(&store), newest);
    task.abort();
    let _ = task.await;

    // An exit while no consumer runs, then a fresh consumer over the same store.
    restart(&run.connection_file, standin).await;
    wait_generation(&run.connection_file, standin, first + 3).await;
    let (source, calls) = Recording::new(Arc::new(spawn_consumer::source::DaemonSource::new(
        run.connection_file.clone(),
    )));
    let (task, _status) = start_consumer(source, census.clone(), &store);
    until("the exit during the downtime is revoked", || {
        census
            .revokes()
            .contains(&(standin.to_string(), Fence::Exited(first + 2)))
            .then_some(())
    })
    .await;
    let subscribed: Vec<_> = calls_of(&calls)
        .into_iter()
        .filter(|call| matches!(call, Call::Subscribe(_)))
        .collect();
    assert_eq!(
        subscribed,
        vec![Call::Subscribe(newest.clone())],
        "the restarted consumer resumes from the recorded cursor"
    );
    assert!(
        census.revokes().iter().all(|(module, _)| module == standin),
        "a spawn asks for nothing: {:?}",
        census.revokes()
    );
    task.abort();
    let _ = task.await;
    run.shutdown().await;
    let _ = std::fs::remove_dir_all(&store);
    passed();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_incarnation_is_refused_byte_exact_and_the_consumer_reconciles_from_a_snapshot() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let run = AcceptanceRun::start(Path::new(env!("CARGO_BIN_EXE_ck-bus"))).await;
    let current = daemon_snapshot(&run.connection_file).await.cursor;
    let foreign = cursor("not-this-daemon", 1);

    let reply = control::rpc(
        &run.connection_file,
        ClientControlRequest::SupervisorSpawnSubscribe {
            since: Some(foreign.clone()),
        },
    )
    .await;
    let control::ControlReply::Error(refusal) = reply else {
        panic!("a foreign cursor must be refused: {reply:?}");
    };
    assert_eq!(refusal.code, "spawn_cursor_incarnation_mismatch");
    assert_eq!(
        refusal.detail,
        Some(serde_json::json!({ "current_daemon_incarnation": current.daemon_incarnation })),
        "the refusal carries exactly its one detail key"
    );

    let store = store_dir("foreign");
    std::fs::write(
        store.join("spawn_cursor.json"),
        serde_json::to_vec(&foreign).unwrap(),
    )
    .unwrap();
    let census = RecordingCensus::with(vec![CensusEntry {
        module_id: "gone".to_string(),
        spawn_generation: 4,
    }]);
    let (source, calls) = Recording::new(Arc::new(spawn_consumer::source::DaemonSource::new(
        run.connection_file.clone(),
    )));
    let (task, status) = start_consumer(source, census.clone(), &store);
    let calls = until("the consumer resubscribes after the refusal", || {
        let calls = calls_of(&calls);
        (calls.len() >= 4).then_some(calls)
    })
    .await;
    let Call::Snapshot(fresh) = &calls[2] else {
        panic!("the refusal is followed by a snapshot: {calls:?}");
    };
    assert!(matches!(&calls[0], Call::Snapshot(_)), "{calls:?}");
    assert_eq!(calls[1], Call::Subscribe(foreign.clone()), "{calls:?}");
    assert_eq!(calls[3], Call::Subscribe(fresh.clone()), "{calls:?}");
    assert_eq!(fresh.daemon_incarnation, current.daemon_incarnation);
    assert_eq!(
        status.lock().unwrap().reconciliations,
        2,
        "the refused cursor is followed by a reconciliation"
    );
    assert_eq!(census.listings(), 2);
    assert_eq!(
        census.revokes(),
        vec![("gone".to_string(), Fence::NotLive(4))],
        "the entry with no live generation is revoked"
    );
    until("the fresh cursor is recorded", || {
        (stored_cursor(&store) == *fresh).then_some(())
    })
    .await;
    task.abort();
    let _ = task.await;
    run.shutdown().await;
    let _ = std::fs::remove_dir_all(&store);
    passed();
}

#[tokio::test]
async fn a_cursor_older_than_the_ring_falls_back_to_a_snapshot_and_reconciles() {
    let store = store_dir("too-old");
    let stale = cursor("inc-a", 1);
    std::fs::write(
        store.join("spawn_cursor.json"),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    let scripted = Scripted::new(
        vec![
            snapshot(cursor("inc-a", 5000), &[("alive", 2)]),
            snapshot(cursor("inc-a", 5003), &[("alive", 2)]),
        ],
        vec![vec![Err(FeedEnd::CursorRefused {
            code: "spawn_cursor_too_old".to_string(),
        })]],
    );
    let census = RecordingCensus::with(vec![
        CensusEntry {
            module_id: "alive".to_string(),
            spawn_generation: 2,
        },
        CensusEntry {
            module_id: "gone".to_string(),
            spawn_generation: 7,
        },
    ]);
    let (source, calls) = Recording::new(scripted);
    let (task, status) = start_consumer(source, census.clone(), &store);
    let calls = until("the consumer resubscribes", || {
        let calls = calls_of(&calls);
        (calls.len() >= 4).then_some(calls)
    })
    .await;
    assert_eq!(
        calls,
        vec![
            Call::Snapshot(cursor("inc-a", 5000)),
            Call::Subscribe(stale),
            Call::Snapshot(cursor("inc-a", 5003)),
            Call::Subscribe(cursor("inc-a", 5003)),
        ]
    );
    assert_eq!(status.lock().unwrap().reconciliations, 2);
    assert_eq!(
        census.revokes(),
        vec![("gone".to_string(), Fence::NotLive(7))],
        "only the entry with no live generation is revoked"
    );
    assert_eq!(stored_cursor(&store), cursor("inc-a", 5003));
    task.abort();
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&store);
    passed();
}

#[tokio::test]
async fn a_lagged_stream_reconciles_and_resubscribes_from_its_last_cursor() {
    let store = store_dir("lagged");
    std::fs::write(
        store.join("spawn_cursor.json"),
        serde_json::to_vec(&cursor("inc-a", 5)).unwrap(),
    )
    .unwrap();
    let scripted = Scripted::new(
        vec![
            snapshot(cursor("inc-a", 5), &[("worker", 2)]),
            snapshot(cursor("inc-a", 9), &[]),
            snapshot(cursor("inc-a", 9000), &[]),
        ],
        vec![
            vec![
                Ok(exited(cursor("inc-a", 6), "worker", 2)),
                Err(FeedEnd::Lagged),
            ],
            // The last cursor is gone from the ring by the time of the resubscription.
            vec![Err(FeedEnd::CursorRefused {
                code: "spawn_cursor_too_old".to_string(),
            })],
        ],
    );
    let census = RecordingCensus::with(vec![CensusEntry {
        module_id: "worker".to_string(),
        spawn_generation: 2,
    }]);
    let (source, calls) = Recording::new(scripted);
    let (task, status) = start_consumer(source, census.clone(), &store);
    let calls = until("the consumer settles on the snapshot", || {
        let calls = calls_of(&calls);
        (calls.len() >= 6).then_some(calls)
    })
    .await;
    assert_eq!(
        calls,
        vec![
            Call::Snapshot(cursor("inc-a", 5)),
            Call::Subscribe(cursor("inc-a", 5)),
            // Lagged: reconcile, then resume from the last event received (6), not the
            // snapshot's cursor (9).
            Call::Snapshot(cursor("inc-a", 9)),
            Call::Subscribe(cursor("inc-a", 6)),
            // That cursor is refused: a fresh snapshot, and its cursor.
            Call::Snapshot(cursor("inc-a", 9000)),
            Call::Subscribe(cursor("inc-a", 9000)),
        ]
    );
    assert_eq!(status.lock().unwrap().reconciliations, 3);
    assert_eq!(
        census.revokes(),
        vec![("worker".to_string(), Fence::Exited(2))],
        "the exit before the lag is revoked by its fact"
    );
    task.abort();
    let _ = task.await;
    let _ = std::fs::remove_dir_all(&store);
    passed();
}
