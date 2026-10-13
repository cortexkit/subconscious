use super::*;
use cortexkit_test_support::ScratchDir;
use std::sync::atomic::{AtomicU64, Ordering};
use subc_protocol::Principal;

fn request() -> ActivityRecordRequest {
    serde_json::from_value(serde_json::json!({"op":"activity.record", "kind":"worker.settled", "summary":"Worker settled"})).unwrap()
}

fn actor() -> ActivityActor {
    ActivityActor {
        principal: Principal::Reserved {
            module_id: "worker".into(),
        },
        scope_owner: None,
        agent_id: None,
        run_id: None,
        flow_id: None,
        project_root: None,
    }
}

async fn record(ledger: &ActivityLedger) -> Result<ActivityEntry, &'static str> {
    ledger.record("worker".into(), request(), actor(), 7).await
}

#[test]
fn activity_kind_is_owned_and_well_formed() {
    for kind in [
        "foreign.settled",
        "worker.",
        "worker.bad-name",
        "worker.UPPER",
        "worker.a.b",
        &format!("worker.{}", "a".repeat(65)),
    ] {
        let mut body = request();
        body.kind = kind.into();
        assert_eq!(
            validate("worker", &body),
            Err(ACTIVITY_KIND_FORBIDDEN),
            "{kind}"
        );
    }
    let mut body = request();
    body.kind = format!("worker.{}", "a".repeat(64));
    assert_eq!(validate("worker", &body), Ok(()));
}

#[test]
fn activity_limits_count_scalars_and_compact_bytes() {
    let mut body = request();
    body.summary = "é".repeat(200);
    assert_eq!(validate("worker", &body), Ok(()));
    body.summary.push('é');
    assert_eq!(validate("worker", &body), Err(ACTIVITY_TOO_LARGE));
    body = request();
    body.details =
        Some(serde_json::from_value(serde_json::json!({"id":"a".repeat(4087)})).unwrap());
    assert_eq!(
        serde_json::to_vec(body.details.as_ref().unwrap())
            .unwrap()
            .len(),
        4096
    );
    assert_eq!(validate("worker", &body), Ok(()));
    body.details
        .as_mut()
        .unwrap()
        .insert("id".into(), serde_json::json!("a".repeat(4088)));
    assert_eq!(validate("worker", &body), Err(ACTIVITY_TOO_LARGE));
    body = request();
    body.subject = Some(subc_control::ActivitySubject {
        subject_type: "repo".into(),
        id: "é".repeat(256),
    });
    assert_eq!(validate("worker", &body), Ok(()));
    body.subject.as_mut().unwrap().id.push('é');
    assert_eq!(validate("worker", &body), Err(ACTIVITY_TOO_LARGE));
    body.subject = Some(subc_control::ActivitySubject {
        subject_type: "a".repeat(33),
        id: "repo-id".into(),
    });
    assert_eq!(validate("worker", &body), Err(ACTIVITY_TOO_LARGE));
}

#[test]
fn activity_empty_summary_and_invalid_subject_are_malformed() {
    let mut body = request();
    body.summary.clear();
    assert_eq!(validate("worker", &body), Err(ACTIVITY_MALFORMED));
    body = request();
    for (subject_type, id) in [("repo", ""), ("Repo", "id"), ("", "id")] {
        body.subject = Some(subc_control::ActivitySubject {
            subject_type: subject_type.into(),
            id: id.into(),
        });
        assert_eq!(validate("worker", &body), Err(ACTIVITY_MALFORMED));
    }
}

#[tokio::test]
async fn activity_ack_is_written_and_sequence_survives_reopen_and_torn_tail() {
    let scratch = ScratchDir::new("activity-sequence");
    let path = scratch.path().join("activity.jsonl");
    let ledger = ActivityLedger::open(path.clone(), "first".into());
    assert_eq!(record(&ledger).await.unwrap().seq, 1);
    let line = std::fs::read_to_string(&path).unwrap();
    let mut older: ActivityEntry = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(older.actor, actor());
    older.seq = 41;
    std::fs::write(
        path.with_extension("jsonl.1"),
        format!("{}\n", serde_json::to_string(&older).unwrap()),
    )
    .unwrap();
    // No append is outstanding when the old sender is dropped.
    drop(ledger);
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"seq\":999")
        .unwrap();
    let ledger = ActivityLedger::open(path.clone(), "second".into());
    assert_eq!(record(&ledger).await.unwrap().seq, 42);
    assert_eq!(ledger.read_errors(), 1);
    let lines = std::fs::read_to_string(&path).unwrap();
    let entries: Vec<ActivityEntry> = lines
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert_eq!(
        entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
        [1, 42]
    );
    assert_eq!(entries[1].daemon_incarnation, "second");
    assert_eq!(record(&ledger).await.unwrap().seq, 43);
}

#[tokio::test]
async fn activity_open_and_write_failures_are_unavailable() {
    let scratch = ScratchDir::new("activity-faults");
    for hooks in [
        WriterHooks {
            open_failure: true,
            ..Default::default()
        },
        WriterHooks {
            write_failure: true,
            ..Default::default()
        },
    ] {
        let ledger = ActivityLedger::start(
            scratch.path().join("activity.jsonl"),
            "fault".into(),
            2,
            hooks,
        );
        assert_eq!(record(&ledger).await, Err(ACTIVITY_UNAVAILABLE));
        assert_eq!(record(&ledger).await, Err(ACTIVITY_UNAVAILABLE));
    }
}

#[tokio::test]
async fn activity_paused_full_queue_refuses_without_acknowledging_unwritten_entries() {
    let scratch = ScratchDir::new("activity-paused");
    let path = scratch.path().join("activity.jsonl");
    let pause = Arc::new((std::sync::Mutex::new(true), std::sync::Condvar::new()));
    let ledger = ActivityLedger::start(
        path.clone(),
        "paused".into(),
        1,
        WriterHooks {
            pause: Some(pause.clone()),
            ..Default::default()
        },
    );
    let (reply, mut received) = oneshot::channel();
    ledger
        .sender
        .try_send(Append {
            module: "worker".into(),
            request: request(),
            actor: actor(),
            at_ms: 7,
            reply: Some(reply),
            lifecycle: None,
        })
        .unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_millis(250), record(&ledger))
            .await
            .expect("full queue must refuse immediately"),
        Err(ACTIVITY_UNAVAILABLE)
    );
    assert_eq!(
        received.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    );
    *pause.0.lock().unwrap() = false;
    pause.1.notify_all();
    assert_eq!(received.await.unwrap().unwrap().seq, 1);
    assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn activity_rate_burst_refill_refusals_and_reconnect_share_one_bucket() {
    let scratch = ScratchDir::new("activity-rate");
    let clock = Arc::new(AtomicU64::new(0));
    let controlled = clock.clone();
    let ledger = ActivityLedger::start(
        scratch.path().join("activity.jsonl"),
        "rate".into(),
        2,
        WriterHooks {
            clock: Some(Arc::new(move || controlled.load(Ordering::SeqCst))),
            ..Default::default()
        },
    );
    for seq in 1..=60 {
        assert_eq!(record(&ledger).await.unwrap().seq, seq);
    }
    assert_eq!(record(&ledger).await, Err(ACTIVITY_RATE_LIMITED));
    let reconnect = ledger.clone();
    assert_eq!(record(&reconnect).await, Err(ACTIVITY_RATE_LIMITED));
    clock.store(99, Ordering::SeqCst);
    assert_eq!(record(&ledger).await, Err(ACTIVITY_RATE_LIMITED));
    clock.store(100, Ordering::SeqCst);
    let mut bad = request();
    bad.summary.clear();
    assert_eq!(
        ledger.record("worker".into(), bad, actor(), 7).await,
        Err(ACTIVITY_MALFORMED)
    );
    assert_eq!(record(&reconnect).await.unwrap().seq, 61);
    for tick in 2..=600 {
        clock.store(tick * 100, Ordering::SeqCst);
        assert_eq!(record(&ledger).await.unwrap().seq, 60 + tick);
    }
    assert_eq!(record(&ledger).await, Err(ACTIVITY_RATE_LIMITED));
}

fn query_request() -> ActivityQueryRequest {
    serde_json::from_value(serde_json::json!({"op":"activity.query","limit":100})).unwrap()
}

async fn append_at(
    ledger: &ActivityLedger,
    module: &str,
    kind: &str,
    agent: &str,
    project: &str,
    at_ms: u64,
) -> ActivityEntry {
    let mut request = request();
    request.kind = format!("{module}.{kind}");
    let mut actor = actor();
    actor.agent_id = Some(agent.into());
    actor.project_root = Some(PathBuf::from(project));
    ledger
        .record(module.into(), request, actor, at_ms)
        .await
        .unwrap()
}

#[tokio::test]
async fn activity_query_filters_each_dimension_and_orders_by_sequence() {
    let scratch = ScratchDir::new("activity-query-filters");
    let ledger = ActivityLedger::open(scratch.path().join("activity.jsonl"), "query".into());
    append_at(&ledger, "worker", "settled", "a", "p", 50).await;
    append_at(&ledger, "worker", "settled", "a", "p", 50).await;
    append_at(&ledger, "worker", "settled", "b", "p", 50).await;
    append_at(&ledger, "worker", "settled", "a", "q", 50).await;
    append_at(&ledger, "other", "settled", "a", "p", 50).await;
    append_at(&ledger, "worker", "changed", "a", "p", 50).await;
    append_at(&ledger, "worker", "settled", "a", "p", 49).await;
    append_at(&ledger, "worker", "settled", "a", "p", 51).await;
    let mut query = query_request();
    query.agent = Some("a".into());
    query.project = Some(PathBuf::from("p"));
    query.module = Some("worker".into());
    query.kind = Some("worker.settled".into());
    query.since_ms = Some(50);
    query.until_ms = Some(50);
    let reply = ledger.query(query).await.unwrap();
    assert_eq!(
        reply
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(reply.cursor, Some(1));
    assert_eq!(
        reply.head,
        ActivityCursor {
            daemon_incarnation: "query".into(),
            seq: 8
        }
    );
    let reply = ledger.query(query_request()).await.unwrap();
    assert_eq!(
        reply
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [8, 7, 6, 5, 4, 3, 2, 1]
    );
    for (query, expected) in [
        (
            ActivityQueryRequest {
                agent: Some("a".into()),
                ..query_request()
            },
            vec![8, 7, 6, 5, 4, 2, 1],
        ),
        (
            ActivityQueryRequest {
                project: Some("p".into()),
                ..query_request()
            },
            vec![8, 7, 6, 5, 3, 2, 1],
        ),
        (
            ActivityQueryRequest {
                module: Some("worker".into()),
                ..query_request()
            },
            vec![8, 7, 6, 4, 3, 2, 1],
        ),
        (
            ActivityQueryRequest {
                kind: Some("worker.settled".into()),
                ..query_request()
            },
            vec![8, 7, 4, 3, 2, 1],
        ),
        (
            ActivityQueryRequest {
                since_ms: Some(50),
                ..query_request()
            },
            vec![8, 6, 5, 4, 3, 2, 1],
        ),
        (
            ActivityQueryRequest {
                until_ms: Some(50),
                ..query_request()
            },
            vec![7, 6, 5, 4, 3, 2, 1],
        ),
    ] {
        let reply = ledger.query(query).await.unwrap();
        assert_eq!(
            reply
                .entries
                .iter()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[tokio::test]
async fn activity_query_pagination_and_handoff_neither_repeat_nor_skip() {
    let scratch = ScratchDir::new("activity-query-pages");
    let ledger = ActivityLedger::open(scratch.path().join("activity.jsonl"), "pages".into());
    for _ in 0..5 {
        record(&ledger).await.unwrap();
    }
    let mut query = query_request();
    query.limit = 2;
    let first = ledger.query(query.clone()).await.unwrap();
    assert_eq!(
        first
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [5, 4]
    );
    assert_eq!(first.cursor, Some(4));
    record(&ledger).await.unwrap();
    query.cursor = first.cursor;
    let second = ledger.query(query.clone()).await.unwrap();
    assert_eq!(
        second
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [3, 2]
    );
    query.cursor = second.cursor;
    let third = ledger.query(query).await.unwrap();
    assert_eq!(
        third
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [1]
    );
    let mut subscription = ledger.subscribe(Some(first.head)).await.unwrap();
    assert_eq!(subscription.replay.pop_front().unwrap().seq, 6);
    assert!(subscription.replay.is_empty());
    record(&ledger).await.unwrap();
    assert_eq!(subscription.receiver.recv().await.unwrap().seq, 7);
    assert!(subscription.receiver.try_recv().is_err());
}

#[tokio::test]
async fn activity_query_capture_pins_head_and_lengths_without_holding_writer() {
    let scratch = ScratchDir::new("activity-query-capture");
    let ledger = ActivityLedger::open(scratch.path().join("activity.jsonl"), "pin".into());
    record(&ledger).await.unwrap();
    let pinned = ledger
        .shared
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .capture_read()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), record(&ledger))
        .await
        .unwrap()
        .unwrap();
    let reply = pinned.read(query_request(), &ledger.read_errors).unwrap();
    assert_eq!(reply.head.seq, 1);
    assert_eq!(
        reply
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [1]
    );
    let empty = ledger
        .query(ActivityQueryRequest {
            limit: 0,
            ..query_request()
        })
        .await
        .unwrap();
    assert!(empty.entries.is_empty());
    assert_eq!(empty.cursor, None);
    assert_eq!(empty.head.seq, 2);
}

#[tokio::test]
async fn activity_query_capture_retains_open_generations_after_pruning() {
    let scratch = ScratchDir::new("activity-query-prune");
    let path = scratch.path().join("activity.jsonl");
    let ledger = ActivityLedger::open(path.clone(), "pin".into());
    let first = record(&ledger).await.unwrap();
    let mut older = first.clone();
    older.seq = 0;
    std::fs::write(
        generation_path(&path, 1),
        format!("{}\n", serde_json::to_string(&older).unwrap()),
    )
    .unwrap();
    let pinned = ledger
        .shared
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .capture_read()
        .unwrap();
    ledger
        .configure(ActivityConfig {
            max_total_mb: 1,
            ..ActivityConfig::default()
        })
        .await
        .unwrap();
    assert!(!generation_path(&path, 1).exists());
    record(&ledger).await.unwrap();
    let reply = pinned.read(query_request(), &ledger.read_errors).unwrap();
    assert_eq!(reply.head.seq, 1);
    assert_eq!(
        reply
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [1, 0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_query_scan_releases_writer_before_reading_files() {
    let scratch = ScratchDir::new("activity-query-unlocked");
    let pause = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
    let ledger = ActivityLedger::start(
        scratch.path().join("activity.jsonl"),
        "scan".into(),
        2,
        WriterHooks {
            read_pause: Some(pause.clone()),
            ..Default::default()
        },
    );
    record(&ledger).await.unwrap();
    let copy = ledger.clone();
    let scan = tokio::spawn(async move { copy.query(query_request()).await });
    pause.0.wait();
    let appended = tokio::time::timeout(std::time::Duration::from_secs(1), record(&ledger)).await;
    pause.1.wait();
    let reply = scan.await.unwrap().unwrap();
    assert_eq!(
        appended
            .expect("append must proceed while the query is reading files")
            .unwrap()
            .seq,
        2
    );
    assert_eq!(reply.head.seq, 1);
    assert_eq!(
        reply
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [1]
    );
}

#[tokio::test]
async fn activity_restart_query_head_replays_only_new_incarnation_entries() {
    let scratch = ScratchDir::new("activity-restart-head");
    let path = scratch.path().join("activity.jsonl");
    let old = ActivityLedger::open(path.clone(), "old".into());
    for _ in 0..3 {
        record(&old).await.unwrap();
    }
    let old_head = old.query(query_request()).await.unwrap().head;
    drop(old);
    let ledger = ActivityLedger::open(path, "new".into());
    let recovered = ledger.query(query_request()).await.unwrap();
    assert_eq!(
        recovered
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [3, 2, 1]
    );
    assert_eq!(
        recovered.head,
        ActivityCursor {
            daemon_incarnation: "new".into(),
            seq: 3
        }
    );
    assert!(matches!(
        ledger.subscribe(Some(old_head)).await,
        Err(ACTIVITY_CURSOR_INCARNATION_MISMATCH)
    ));
    let mut subscription = ledger
        .subscribe(Some(recovered.head.clone()))
        .await
        .unwrap();
    assert!(subscription.replay.is_empty());
    assert_eq!(record(&ledger).await.unwrap().seq, 4);
    assert_eq!(subscription.receiver.recv().await.unwrap().seq, 4);
    assert!(subscription.receiver.try_recv().is_err());
    let mut reconnect = ledger.subscribe(Some(recovered.head)).await.unwrap();
    assert_eq!(reconnect.replay.pop_front().unwrap().seq, 4);
    assert!(reconnect.replay.is_empty());
    let fresh = ledger.subscribe(None).await.unwrap();
    assert!(fresh.replay.is_empty());
}

#[tokio::test]
async fn activity_ring_refuses_old_cursor_and_query_recovers_all_retained() {
    let scratch = ScratchDir::new("activity-ring-recovery");
    let clock = Arc::new(AtomicU64::new(0));
    let controlled = clock.clone();
    let ledger = ActivityLedger::start(
        scratch.path().join("activity.jsonl"),
        "ring".into(),
        2,
        WriterHooks {
            clock: Some(Arc::new(move || controlled.load(Ordering::Relaxed))),
            ..Default::default()
        },
    );
    for tick in 1..=(RING_CAPACITY as u64 + 2) {
        clock.store(tick * 100, Ordering::Relaxed);
        record(&ledger).await.unwrap();
    }
    let cursor = |seq| ActivityCursor {
        daemon_incarnation: "ring".into(),
        seq,
    };
    assert!(matches!(
        ledger.subscribe(Some(cursor(1))).await,
        Err(ACTIVITY_CURSOR_TOO_OLD)
    ));
    let replay = ledger.subscribe(Some(cursor(2))).await.unwrap();
    assert_eq!(replay.replay.front().unwrap().seq, 3);
    assert_eq!(replay.replay.len(), RING_CAPACITY);
    let recovered = ledger
        .query(ActivityQueryRequest {
            limit: u32::MAX,
            ..query_request()
        })
        .await
        .unwrap();
    assert_eq!(recovered.entries.len(), RING_CAPACITY + 2);
    let mut subscription = ledger.subscribe(Some(recovered.head)).await.unwrap();
    assert!(subscription.replay.is_empty());
    clock.fetch_add(100, Ordering::Relaxed);
    let appended = record(&ledger).await.unwrap();
    assert_eq!(
        subscription.receiver.recv().await.unwrap().seq,
        appended.seq
    );
}

#[tokio::test]
async fn activity_restart_predecessor_cursor_requires_requery_after_ring_eviction() {
    let scratch = ScratchDir::new("activity-predecessor-eviction");
    let path = scratch.path().join("activity.jsonl");
    let old = ActivityLedger::open(path.clone(), "old".into());
    for _ in 0..40 {
        record(&old).await.unwrap();
    }
    drop(old);
    let clock = Arc::new(AtomicU64::new(0));
    let controlled = clock.clone();
    let ledger = ActivityLedger::start(
        path,
        "new".into(),
        2,
        WriterHooks {
            clock: Some(Arc::new(move || controlled.load(Ordering::Relaxed))),
            ..Default::default()
        },
    );
    let head = ledger.query(query_request()).await.unwrap().head;
    assert_eq!(head.seq, 40);
    for tick in 1..=1100 {
        clock.store(tick * 100, Ordering::Relaxed);
        record(&ledger).await.unwrap();
    }
    assert!(matches!(
        ledger.subscribe(Some(head)).await,
        Err(ACTIVITY_CURSOR_TOO_OLD)
    ));
    let recovered = ledger
        .query(ActivityQueryRequest {
            limit: u32::MAX,
            ..query_request()
        })
        .await
        .unwrap();
    assert_eq!(
        recovered
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        (1..=1140).rev().collect::<Vec<_>>()
    );
    let mut follow = ledger.subscribe(Some(recovered.head)).await.unwrap();
    assert!(follow.replay.is_empty());
    clock.fetch_add(100, Ordering::Relaxed);
    assert_eq!(record(&ledger).await.unwrap().seq, 1141);
    assert_eq!(follow.receiver.recv().await.unwrap().seq, 1141);
    assert!(follow.receiver.try_recv().is_err());
}

#[tokio::test]
async fn activity_lag_drains_queued_entries_before_terminal() {
    let scratch = ScratchDir::new("activity-lag");
    let clock = Arc::new(AtomicU64::new(0));
    let controlled = clock.clone();
    let ledger = ActivityLedger::start(
        scratch.path().join("activity.jsonl"),
        "lag".into(),
        2,
        WriterHooks {
            clock: Some(Arc::new(move || controlled.load(Ordering::Relaxed))),
            ..Default::default()
        },
    );
    let mut subscription = ledger.subscribe(None).await.unwrap();
    for tick in 1..=(SUBSCRIBER_CAPACITY as u64 + 1) {
        clock.store(tick * 100, Ordering::Relaxed);
        record(&ledger).await.unwrap();
    }
    for seq in 1..=SUBSCRIBER_CAPACITY as u64 {
        assert_eq!(subscription.receiver.recv().await.unwrap().seq, seq);
    }
    assert!(subscription.receiver.recv().await.is_none());
    assert_eq!(
        subscription.terminal_code(),
        Some(ACTIVITY_SUBSCRIBER_LAGGED)
    );
}

#[tokio::test]
async fn activity_subscriber_never_observes_a_failed_or_paused_append() {
    let scratch = ScratchDir::new("activity-feed-write");
    let ledger = ActivityLedger::start(
        scratch.path().join("failed.jsonl"),
        "failed".into(),
        2,
        WriterHooks {
            write_failure: true,
            ..Default::default()
        },
    );
    let mut subscriber = ledger.subscribe(None).await.unwrap();
    assert_eq!(record(&ledger).await, Err(ACTIVITY_UNAVAILABLE));
    assert!(subscriber.receiver.try_recv().is_err());
    let pause = Arc::new((Mutex::new(true), Condvar::new()));
    let ledger = ActivityLedger::start(
        scratch.path().join("paused.jsonl"),
        "paused".into(),
        2,
        WriterHooks {
            pause: Some(pause.clone()),
            ..Default::default()
        },
    );
    let mut subscriber = ledger.subscribe(None).await.unwrap();
    let copy = ledger.clone();
    let pending = tokio::spawn(async move { record(&copy).await });
    while ledger.remaining_capacity() == 2 {
        tokio::task::yield_now().await;
    }
    assert!(subscriber.receiver.try_recv().is_err());
    *pause.0.lock().unwrap() = false;
    pause.1.notify_all();
    assert_eq!(pending.await.unwrap().unwrap().seq, 1);
    assert_eq!(subscriber.receiver.recv().await.unwrap().seq, 1);
}

#[tokio::test]
async fn activity_retention_reopens_and_prunes_size_and_age_without_restart() {
    let scratch = ScratchDir::new("activity-retention");
    let path = scratch.path().join("activity.jsonl");
    let ledger = ActivityLedger::open(path.clone(), "retain".into());
    let first = record(&ledger).await.unwrap();
    assert_eq!(
        ActivityConfig::default(),
        ActivityConfig {
            max_age_days: 30,
            max_total_mb: 64
        }
    );
    let retention = ActivityConfig::default().retention();
    assert_eq!(
        (
            retention.max_file_mb,
            retention.keep,
            retention.max_age_days
        ),
        (1, 63, 30)
    );
    let large = ActivityConfig {
        max_age_days: 30,
        max_total_mb: 1024,
    }
    .retention();
    assert_eq!((large.max_file_mb, large.keep), (4, 255));
    for generation in 1..=4 {
        let mut entry = first.clone();
        entry.seq = 10 - u64::from(generation);
        std::fs::write(
            generation_path(&path, generation),
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
    }
    ledger
        .configure(ActivityConfig {
            max_age_days: 30,
            max_total_mb: 2,
        })
        .await
        .unwrap();
    assert!(generation_path(&path, 1).exists());
    assert!(!generation_path(&path, 2).exists());
    assert!(!generation_path(&path, 3).exists());
    assert!(!generation_path(&path, 4).exists());
    let old = File::options()
        .write(true)
        .open(generation_path(&path, 1))
        .unwrap();
    old.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400))
        .unwrap();
    drop(old);
    ledger
        .configure(ActivityConfig {
            max_age_days: 1,
            max_total_mb: 2,
        })
        .await
        .unwrap();
    assert!(!generation_path(&path, 1).exists());
    assert_eq!(record(&ledger).await.unwrap().seq, 2);
}

#[tokio::test]
async fn activity_query_counts_corrupt_lines_and_repairs_torn_tail() {
    let scratch = ScratchDir::new("activity-query-corrupt");
    let path = scratch.path().join("activity.jsonl");
    let ledger = ActivityLedger::open(path.clone(), "first".into());
    let entry = record(&ledger).await.unwrap();
    drop(ledger);
    std::fs::write(
        &path,
        format!(
            "not-json\n{}\n{{\"seq\":999",
            serde_json::to_string(&entry).unwrap()
        ),
    )
    .unwrap();
    let ledger = ActivityLedger::open(path.clone(), "second".into());
    let reply = ledger.query(query_request()).await.unwrap();
    assert_eq!(
        reply
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(ledger.read_errors(), 4);
    assert!(std::fs::read(&path).unwrap().ends_with(b"\n"));
    assert_eq!(record(&ledger).await.unwrap().seq, 2);
    let reply = ledger.query(query_request()).await.unwrap();
    assert_eq!(
        reply
            .entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(ledger.read_errors(), 6);
}
