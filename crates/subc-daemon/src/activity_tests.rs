use super::*;
use cortexkit_test_support::ScratchDir;
use std::sync::atomic::{AtomicU64, Ordering};
use subc_protocol::Principal;

fn request() -> ActivityRecordRequest {
    serde_json::from_value(serde_json::json!({"op":"activity.record", "kind":"worker.settled", "summary":"Worker settled"})).unwrap()
}

#[test]
fn activity_daemon_release_has_a_version_and_changelog_bump() {
    // Adding the activity.record operation changes the published daemon API;
    // the crate and its changelog must therefore move beyond release 0.33.8.
    let version = env!("CARGO_PKG_VERSION");
    let components: Vec<u64> = version
        .split('.')
        .map(|part| part.parse().unwrap())
        .collect();
    assert!(components.as_slice() >= [0, 33, 9].as_slice());
    assert!(include_str!("../CHANGELOG.md").contains(&format!("\n## {version}\n")));
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
            reply,
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
