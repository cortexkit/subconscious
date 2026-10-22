use super::*;
use cortexkit_test_support::ScratchDir;
use subc_control::{
    ActivityQueryRequest, RunningImageAgreement, RunningImageEvidence,
    RunningImageUnavailableReason,
};
use subc_protocol::Principal;

fn image(digest: &str) -> RunningImageAgreement {
    RunningImageAgreement::Match {
        evidence: RunningImageEvidence::LinuxProcSha256 {
            digest: digest.into(),
        },
    }
}

fn query() -> ActivityQueryRequest {
    serde_json::from_value(serde_json::json!({"op":"activity.query", "limit":1000})).unwrap()
}

async fn flush(ledger: &ActivityLedger) {
    ledger.record("barrier".into(), serde_json::from_value(serde_json::json!({"op":"activity.record", "kind":"barrier.flushed", "summary":"Queue flushed"})).unwrap(), ActivityActor { principal: Principal::Direct, scope_owner: None, agent_id: None, run_id: None, flow_id: None, project_root: None, workdir: None, project_id: None }, 1).await.unwrap();
}

async fn starts(ledger: &ActivityLedger) -> Vec<ActivityEntry> {
    flush(ledger).await;
    let mut request = query();
    request.kind = Some("subc.module_started".into());
    ledger.query(request).await.unwrap().entries
}

#[tokio::test]
async fn lifecycle_started_compares_latest_retained_digest_and_defaults_to_subc() {
    let home = ScratchDir::new("lifecycle-build");
    let path = home.path().join("activity.jsonl");
    let ledger = ActivityLedger::open(path.clone(), "first".into());
    ledger.module_started("worker", image("build-a"), None);
    let initial = starts(&ledger).await;
    assert_eq!(initial.len(), 1);
    assert_eq!(
        initial[0].actor.principal,
        Principal::Reserved {
            module_id: "subc".into()
        }
    );
    assert_eq!(initial[0].module, "subc");
    assert_eq!(initial[0].subject.as_ref().unwrap().id, "worker");
    assert_eq!(
        initial[0].details,
        serde_json::json!({"old_evidence":null,"new_evidence":{"method":"linux_proc_sha256","digest":"build-a"}})
            .as_object()
            .unwrap()
            .clone()
    );
    ledger.module_started("worker", image("build-a"), None);
    assert_eq!(starts(&ledger).await.len(), 1, "same build must be a no-op");
    // Different module ids have independent retained build identities.
    ledger.module_started("other", image("build-a"), None);
    ledger.module_started("worker", image("build-b"), None);
    assert_eq!(starts(&ledger).await.len(), 3);
    drop(ledger);
    let ledger = ActivityLedger::open(path, "second".into());
    ledger.module_started("worker", image("build-b"), None);
    assert_eq!(
        starts(&ledger).await.len(),
        3,
        "retained build survives daemon restart"
    );
    let requester = Principal::Reserved {
        module_id: "operator-module".into(),
    };
    ledger.module_started("worker", image("build-a"), Some(requester.clone()));
    let entries = starts(&ledger).await;
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].actor.principal, requester);
    assert_eq!(
        entries[0].details,
        serde_json::json!({"old_evidence":{"method":"linux_proc_sha256","digest":"build-b"},"new_evidence":{"method":"linux_proc_sha256","digest":"build-a"}})
            .as_object()
            .unwrap()
            .clone()
    );
}

#[tokio::test]
async fn lifecycle_unmeasurable_digest_skips_and_counts_without_fabricating_a_build() {
    let home = ScratchDir::new("lifecycle-no-digest");
    let ledger = ActivityLedger::open(home.path().join("activity.jsonl"), "skip".into());
    ledger.module_started(
        "worker",
        RunningImageAgreement::Unavailable {
            reason: RunningImageUnavailableReason::NotRunning,
        },
        None,
    );
    ledger.module_started(
        "worker",
        RunningImageAgreement::Unavailable {
            reason: RunningImageUnavailableReason::ProcessIdentityUnconfirmed,
        },
        None,
    );
    assert!(starts(&ledger).await.is_empty());
    assert_eq!(ledger.lifecycle_digest_skips(), 2);
    assert_eq!(ledger.lifecycle_failures(), 0);
    // A mismatch still measures the running image, not the replacement on disk.
    ledger.module_started(
        "worker",
        RunningImageAgreement::Mismatch {
            running: RunningImageEvidence::LinuxProcSha256 {
                digest: "running".into(),
            },
            disk: RunningImageEvidence::LinuxProcSha256 {
                digest: "disk".into(),
            },
        },
        None,
    );
    assert_eq!(
        starts(&ledger).await[0].details["new_evidence"],
        serde_json::json!({"method":"linux_proc_sha256", "digest":"running"})
    );
}

#[tokio::test]
async fn lifecycle_is_rate_exempt_after_subc_bucket_is_exhausted() {
    let home = ScratchDir::new("lifecycle-rate");
    let ledger = ActivityLedger::start(
        home.path().join("activity.jsonl"),
        "rate".into(),
        64,
        WriterHooks {
            clock: Some(Arc::new(|| 0)),
            ..Default::default()
        },
    );
    let request: ActivityRecordRequest = serde_json::from_value(serde_json::json!({"op":"activity.record", "kind":"subc.ordinary", "summary":"Ordinary event"})).unwrap();
    let actor = ActivityActor {
        principal: Principal::Direct,
        scope_owner: None,
        agent_id: None,
        run_id: None,
        flow_id: None,
        project_root: None,
        workdir: None,
        project_id: None,
    };
    for _ in 0..60 {
        ledger
            .record("subc".into(), request.clone(), actor.clone(), 1)
            .await
            .unwrap();
    }
    assert_eq!(
        ledger.record("subc".into(), request, actor, 1).await,
        Err(ACTIVITY_RATE_LIMITED)
    );
    ledger.module_transition("subc.module_disabled", "worker", Principal::Direct);
    flush(&ledger).await;
    let mut request = query();
    request.kind = Some("subc.module_disabled".into());
    assert_eq!(ledger.query(request).await.unwrap().entries.len(), 1);
    assert_eq!(ledger.lifecycle_failures(), 0);
}

#[tokio::test]
async fn lifecycle_rescan_records_only_roster_changes_with_exact_ids() {
    let home = ScratchDir::new("lifecycle-roster");
    let ledger = ActivityLedger::open(home.path().join("activity.jsonl"), "rescan".into());
    ledger.module_rescanned(&[], &[], Principal::Direct);
    flush(&ledger).await;
    let mut request = query();
    request.kind = Some("subc.module_rescanned".into());
    assert!(ledger
        .query(request.clone())
        .await
        .unwrap()
        .entries
        .is_empty());
    ledger.module_rescanned(&["new".into()], &["old".into()], Principal::Direct);
    flush(&ledger).await;
    let entries = ledger.query(request).await.unwrap().entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].details,
        serde_json::json!({"added":["new"],"removed":["old"]})
            .as_object()
            .unwrap()
            .clone()
    );
}

#[tokio::test]
async fn lifecycle_open_write_and_queue_drops_increment_failure_count() {
    let home = ScratchDir::new("lifecycle-failures");
    for (index, hooks) in [
        WriterHooks {
            open_failure: true,
            ..Default::default()
        },
        WriterHooks {
            write_failure: true,
            ..Default::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let ledger = ActivityLedger::start(
            home.path().join(format!("fault-{index}.jsonl")),
            "fault".into(),
            4,
            hooks,
        );
        ledger.module_transition("subc.module_disabled", "worker", Principal::Direct);
        let request = serde_json::from_value(serde_json::json!({"op":"activity.record", "kind":"barrier.flushed", "summary":"Queue flushed"})).unwrap();
        let actor = ActivityActor {
            principal: Principal::Direct,
            scope_owner: None,
            agent_id: None,
            run_id: None,
            flow_id: None,
            project_root: None,
            workdir: None,
            project_id: None,
        };
        assert_eq!(
            ledger.record("barrier".into(), request, actor, 1).await,
            Err(ACTIVITY_UNAVAILABLE)
        );
        assert_eq!(ledger.lifecycle_failures(), 1);
    }
    let pause = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
    let ledger = ActivityLedger::start(
        home.path().join("paused.jsonl"),
        "paused".into(),
        1,
        WriterHooks {
            pause: Some(pause.clone()),
            ..Default::default()
        },
    );
    ledger.module_transition("subc.module_disabled", "worker", Principal::Direct);
    ledger.module_transition("subc.module_stopped", "worker", Principal::Direct);
    assert_eq!(
        ledger.lifecycle_failures(),
        1,
        "the full queue counts the dropped kind immediately"
    );
    *pause.0.lock().unwrap() = false;
    pause.1.notify_all();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while ledger.remaining_capacity() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    flush(&ledger).await;
    assert_eq!(ledger.lifecycle_failures(), 1);
}

#[tokio::test]
async fn lifecycle_unmeasurable_registration_uses_writer_skip_counter() {
    let home = ScratchDir::new("lifecycle-unmeasurable-registration");
    let ledger = ActivityLedger::open(home.path().join("activity.jsonl"), "unmeasurable".into());
    let observation = Arc::new(RegistrationImage::new(
        crate::provenance::ExecutableIdentityProbe::default(),
    ));
    // An accepted HELLO may precede spawn bookkeeping. The writer, not the
    // connection reader, waits for the pinned observation to become available.
    ledger.module_registered("worker", None, observation.clone());
    observation.complete(Some(ImageFacts {
        pid: None,
        path: None,
        identity: None,
        start: None,
        exec: {
            let exec = Arc::new(ExecOutcome::new());
            exec.finish(ExecState::CommittedExec);
            exec
        },
    }));
    assert!(starts(&ledger).await.is_empty());
    assert_eq!(ledger.lifecycle_digest_skips(), 1);
    assert_eq!(ledger.lifecycle_failures(), 0);
}

async fn assert_platform_evidence_changes(cases: Vec<(RunningImageEvidence, serde_json::Value)>) {
    let home = ScratchDir::new("lifecycle-platform-evidence");
    let path = home.path().join("activity.jsonl");
    let ledger = ActivityLedger::open(path.clone(), "first".into());
    let mut old = serde_json::Value::Null;
    for (index, (evidence, expected)) in cases.iter().enumerate() {
        ledger.module_started(
            "worker",
            RunningImageAgreement::Match {
                evidence: evidence.clone(),
            },
            None,
        );
        let entries = starts(&ledger).await;
        assert_eq!(
            entries.len(),
            index + 1,
            "a changed executable identity must record a start"
        );
        assert_eq!(entries[0].details["old_evidence"], old);
        assert_eq!(entries[0].details["new_evidence"], *expected);
        ledger.module_started(
            "worker",
            RunningImageAgreement::Match {
                evidence: evidence.clone(),
            },
            None,
        );
        assert_eq!(
            starts(&ledger).await.len(),
            index + 1,
            "restarting the same executable identity must be silent"
        );
        old = expected.clone();
    }
    assert_eq!(
        ledger.lifecycle_digest_skips(),
        0,
        "platform evidence is available evidence"
    );
    assert_eq!(ledger.lifecycle_failures(), 0);
    drop(ledger);
    let ledger = ActivityLedger::open(path, "second".into());
    ledger.module_started(
        "worker",
        RunningImageAgreement::Match {
            evidence: cases.last().unwrap().0.clone(),
        },
        None,
    );
    assert_eq!(
        starts(&ledger).await.len(),
        cases.len(),
        "retained platform identity survives daemon restart"
    );
}

#[tokio::test]
async fn lifecycle_macos_evidence_changes_record_starts_and_unchanged_restarts_are_silent() {
    assert_platform_evidence_changes(vec![
        (
            RunningImageEvidence::MacosSpawnInode {
                device: 11,
                inode: 21,
            },
            serde_json::json!({"method":"macos_spawn_inode", "device":11, "inode":21}),
        ),
        (
            RunningImageEvidence::MacosSpawnInode {
                device: 11,
                inode: 22,
            },
            serde_json::json!({"method":"macos_spawn_inode", "device":11, "inode":22}),
        ),
        (
            RunningImageEvidence::MacosSpawnInode {
                device: 12,
                inode: 22,
            },
            serde_json::json!({"method":"macos_spawn_inode", "device":12, "inode":22}),
        ),
    ])
    .await;
}

#[tokio::test]
async fn lifecycle_windows_evidence_changes_record_starts_and_unchanged_restarts_are_silent() {
    assert_platform_evidence_changes(vec![
        (RunningImageEvidence::WindowsSpawnFileId { volume_serial_number: 13, file_id: [1; 16] }, serde_json::json!({"method":"windows_spawn_file_id", "volume_serial_number":13, "file_id":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1]})),
        (RunningImageEvidence::WindowsSpawnFileId { volume_serial_number: 13, file_id: [2; 16] }, serde_json::json!({"method":"windows_spawn_file_id", "volume_serial_number":13, "file_id":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2]})),
        (RunningImageEvidence::WindowsSpawnFileId { volume_serial_number: 14, file_id: [2; 16] }, serde_json::json!({"method":"windows_spawn_file_id", "volume_serial_number":14, "file_id":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2]})),
    ]).await;
}

#[tokio::test]
async fn lifecycle_changed_evidence_method_counts_as_a_new_build() {
    assert_platform_evidence_changes(vec![
        (RunningImageEvidence::LinuxProcSha256 { digest: "42".into() }, serde_json::json!({"method":"linux_proc_sha256", "digest":"42"})),
        (RunningImageEvidence::MacosSpawnInode { device: 42, inode: 42 }, serde_json::json!({"method":"macos_spawn_inode", "device":42, "inode":42})),
        (RunningImageEvidence::WindowsSpawnFileId { volume_serial_number: 42, file_id: [42; 16] }, serde_json::json!({"method":"windows_spawn_file_id", "volume_serial_number":42, "file_id":[42,42,42,42,42,42,42,42,42,42,42,42,42,42,42,42]})),
    ]).await;
}
