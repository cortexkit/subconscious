use super::*;
use cortexkit_test_support::ScratchDir;
use std::{collections::VecDeque, sync::Mutex};
use subc_client_rs::{ActivityActor, ActivityQueryResponse};
use subc_protocol::Principal;
use tokio::{sync::Notify, time::timeout};

fn cursor(incarnation: &str, seq: u64) -> ActivityCursor {
    ActivityCursor {
        daemon_incarnation: incarnation.into(),
        seq,
    }
}
fn entry(incarnation: &str, seq: u64) -> ActivityEntry {
    ActivityEntry {
        seq,
        at_ms: 50,
        daemon_incarnation: incarnation.into(),
        module: "worker".into(),
        kind: "worker.settled".into(),
        summary: format!("Entry {seq}"),
        subject: None,
        details: Default::default(),
        actor: ActivityActor {
            principal: Principal::Direct,
            scope_owner: None,
            agent_id: None,
            run_id: None,
            flow_id: None,
            project_root: None,
        },
    }
}

#[derive(Default)]
struct Ledger {
    entries: Mutex<Vec<ActivityEntry>>,
    incarnation: Mutex<String>,
    queries: Mutex<Vec<Option<u64>>>,
    subscriptions: Mutex<Vec<ActivityCursor>>,
    ends: Mutex<VecDeque<FeedEnd>>,
    handoff_append: Mutex<Option<ActivityEntry>>,
}
impl Ledger {
    fn new(entries: Vec<ActivityEntry>) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(entries),
            incarnation: Mutex::new("current".into()),
            ..Default::default()
        })
    }
    fn head(&self) -> ActivityCursor {
        cursor(
            &self.incarnation.lock().unwrap(),
            self.entries
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.seq)
                .max()
                .unwrap_or(0),
        )
    }
}
struct Replay {
    entries: VecDeque<ActivityEntry>,
    end: Option<FeedEnd>,
}
#[async_trait]
impl Feed for Replay {
    async fn next(&mut self) -> Result<Option<ActivityEntry>, FeedEnd> {
        if let Some(entry) = self.entries.pop_front() {
            return Ok(Some(entry));
        }
        if let Some(end) = self.end.take() {
            return Err(end);
        }
        std::future::pending().await
    }
}
#[async_trait]
impl Source for Ledger {
    async fn query(
        &self,
        before: Option<u64>,
        limit: u32,
    ) -> Result<ActivityQueryResponse, String> {
        self.queries.lock().unwrap().push(before);
        let head = self.head();
        let mut all: Vec<_> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| before.is_none_or(|seq| e.seq < seq))
            .cloned()
            .collect();
        all.sort_by_key(|e| std::cmp::Reverse(e.seq));
        let size = (limit as usize).min(2);
        let more = all.len() > size;
        all.truncate(size);
        let continuation = if more {
            all.last().map(|e| e.seq)
        } else {
            None
        };
        if before.is_none() {
            if let Some(entry) = self.handoff_append.lock().unwrap().take() {
                self.entries.lock().unwrap().push(entry);
            }
        }
        Ok(ActivityQueryResponse {
            entries: all,
            cursor: continuation,
            head,
        })
    }
    async fn subscribe(&self, since: ActivityCursor) -> Result<Box<dyn Feed>, FeedEnd> {
        self.subscriptions.lock().unwrap().push(since.clone());
        if since.daemon_incarnation != *self.incarnation.lock().unwrap() {
            return Err(FeedEnd::CursorRefused);
        }
        let end = self.ends.lock().unwrap().pop_front();
        let entries = if end.is_some() {
            VecDeque::new()
        } else {
            let mut entries: Vec<_> = self
                .entries
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.seq > since.seq)
                .cloned()
                .collect();
            entries.sort_by_key(|e| e.seq);
            entries.into()
        };
        Ok(Box::new(Replay { entries, end }))
    }
}
#[derive(Default)]
struct Bus {
    accepted: Mutex<Vec<ActivityEntry>>,
    attempts: Mutex<Vec<u64>>,
    fail: Mutex<Option<u64>>,
    hold: Mutex<bool>,
    stored: Notify,
}
#[async_trait]
impl Publisher for Bus {
    async fn publish(&self, entry: &ActivityEntry) -> Result<(), String> {
        self.attempts.lock().unwrap().push(entry.seq);
        if *self.fail.lock().unwrap() == Some(entry.seq) {
            return Err("injected publish failure".into());
        }
        self.accepted.lock().unwrap().push(entry.clone());
        self.stored.notify_one();
        if *self.hold.lock().unwrap() {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}
#[derive(Default)]
struct Events(Mutex<Vec<(String, Value)>>);
impl Journal for Events {
    fn event(&self, name: &str, fields: Value) {
        self.0.lock().unwrap().push((name.into(), fields));
    }
}
fn mirror(ledger: Arc<Ledger>, bus: Arc<Bus>, root: &Path) -> Mirror {
    Mirror::new(ledger, bus, root, Duration::from_secs(1))
}
pub(super) async fn until(mut ready: impl FnMut() -> bool) {
    timeout(Duration::from_secs(20), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observable mirror progress");
}
async fn stop(task: tokio::task::JoinHandle<()>) {
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

#[test]
fn activity_cursor_uses_durable_sibling_replacement_and_classifies_damage() {
    let root = ScratchDir::new("activity-cursor");
    let store = CursorStore::new(&root);
    assert_eq!(store.path(), root.join("activity_cursor.json"));
    assert_eq!(store.read(), CursorRead::Absent);
    let stale = root.join("activity_cursor.json.tmp");
    std::fs::write(&stale, b"uncommitted").unwrap();
    store.remove_stale_tmp();
    assert!(!stale.exists());
    store.write(&cursor("daemon", 5)).unwrap();
    assert_eq!(store.read(), CursorRead::Present(cursor("daemon", 5)));
    assert!(
        !stale.exists(),
        "replacement must consume its sibling temp file"
    );
    std::fs::write(store.path(), b"{torn").unwrap();
    assert!(matches!(store.read(), CursorRead::Damaged(_)));
    std::fs::write(store.path(), br#"{"daemon_incarnation":"","seq":5}"#).unwrap();
    assert!(matches!(store.read(), CursorRead::Damaged(_)));
}

#[tokio::test]
async fn absent_or_damaged_activity_cursor_starts_at_head_without_backfill() {
    for damage in [false, true] {
        let root = ScratchDir::new("activity-baseline");
        let ledger = Ledger::new(vec![entry("previous", 4), entry("current", 5)]);
        let bus = Arc::new(Bus::default());
        let store = CursorStore::new(&root);
        if damage {
            std::fs::write(store.path(), b"{torn").unwrap();
        }
        let mirror = mirror(ledger.clone(), bus.clone(), &root);
        assert_eq!(mirror.load().await.unwrap(), cursor("current", 5));
        assert_eq!(*ledger.queries.lock().unwrap(), [None]);
        assert_eq!(store.read(), CursorRead::Present(cursor("current", 5)));
        assert!(
            bus.accepted.lock().unwrap().is_empty(),
            "history is not a fresh mirror's responsibility"
        );
    }
}

#[tokio::test]
async fn successful_activity_publish_checkpoints_and_restart_resumes() {
    let root = ScratchDir::new("activity-resume");
    let store = CursorStore::new(&root);
    store.write(&cursor("current", 1)).unwrap();
    let ledger = Ledger::new(vec![entry("current", 1), entry("current", 2)]);
    let bus = Arc::new(Bus::default());
    let task = tokio::spawn(mirror(ledger.clone(), bus.clone(), &root).run());
    until(|| store.read() == CursorRead::Present(cursor("current", 2))).await;
    stop(task).await;
    ledger.entries.lock().unwrap().push(entry("current", 3));
    let task = tokio::spawn(mirror(ledger.clone(), bus.clone(), &root).run());
    until(|| store.read() == CursorRead::Present(cursor("current", 3))).await;
    assert_eq!(
        *bus.accepted.lock().unwrap(),
        [entry("current", 2), entry("current", 3)]
    );
    assert_eq!(
        *ledger.subscriptions.lock().unwrap(),
        [cursor("current", 1), cursor("current", 2)]
    );
    assert!(
        ledger.queries.lock().unwrap().is_empty(),
        "valid saved cursor must not be replaced with head"
    );
    stop(task).await;
}

#[tokio::test]
async fn activity_publish_failure_then_restart_loses_nothing_and_backs_off() {
    let root = ScratchDir::new("activity-publish-failure");
    let store = CursorStore::new(&root);
    store.write(&cursor("current", 1)).unwrap();
    let ledger = Ledger::new(vec![entry("current", 2), entry("current", 3)]);
    let bus = Arc::new(Bus::default());
    *bus.fail.lock().unwrap() = Some(2);
    let events = Arc::new(Events::default());
    let mut first = mirror(ledger.clone(), bus.clone(), &root);
    first.journal = events.clone();
    let task = tokio::spawn(first.run());
    until(|| !events.0.lock().unwrap().is_empty()).await;
    // All file I/O has finished before paused time checks the retry boundary.
    tokio::time::pause();
    assert_eq!(store.read(), CursorRead::Present(cursor("current", 1)));
    assert_eq!(*bus.attempts.lock().unwrap(), [2]);
    tokio::time::advance(Duration::from_millis(999)).await;
    tokio::task::yield_now().await;
    assert!(
        events
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|(name, _)| name != "ckbus.activity.retrying"),
        "no retry may start inside the backoff interval"
    );
    assert_eq!(
        *bus.attempts.lock().unwrap(),
        [2],
        "publish failure must not spin"
    );
    stop(task).await;
    tokio::time::resume();
    *bus.fail.lock().unwrap() = None;
    let task = tokio::spawn(mirror(ledger.clone(), bus.clone(), &root).run());
    until(|| store.read() == CursorRead::Present(cursor("current", 3))).await;
    assert_eq!(
        *bus.accepted.lock().unwrap(),
        [entry("current", 2), entry("current", 3)]
    );
    assert_eq!(
        *ledger.subscriptions.lock().unwrap(),
        [cursor("current", 1), cursor("current", 1)]
    );
    stop(task).await;
}

#[tokio::test]
async fn crash_after_activity_publish_before_checkpoint_duplicates_one_identity() {
    let root = ScratchDir::new("activity-crash-window");
    let store = CursorStore::new(&root);
    store.write(&cursor("current", 1)).unwrap();
    let ledger = Ledger::new(vec![entry("current", 2), entry("current", 3)]);
    let bus = Arc::new(Bus::default());
    *bus.hold.lock().unwrap() = true;
    let task = tokio::spawn(mirror(ledger.clone(), bus.clone(), &root).run());
    bus.stored.notified().await;
    // The broker has accepted the entry; the publisher is held before it can
    // return to checkpointing, exactly the storage/checkpoint crash window.
    assert_eq!(store.read(), CursorRead::Present(cursor("current", 1)));
    stop(task).await;
    *bus.hold.lock().unwrap() = false;
    let task = tokio::spawn(mirror(ledger, bus.clone(), &root).run());
    until(|| store.read() == CursorRead::Present(cursor("current", 3))).await;
    assert_eq!(
        *bus.accepted.lock().unwrap(),
        [
            entry("current", 2),
            entry("current", 2),
            entry("current", 3)
        ]
    );
    stop(task).await;
}

#[tokio::test]
async fn activity_refused_cursor_queries_pages_ascending_and_follows_head() {
    let root = ScratchDir::new("activity-refused");
    let store = CursorStore::new(&root);
    store.write(&cursor("previous", 1)).unwrap();
    let ledger = Ledger::new(
        (1..=7)
            .map(|seq| entry(if seq <= 4 { "previous" } else { "current" }, seq))
            .collect(),
    );
    *ledger.handoff_append.lock().unwrap() = Some(entry("current", 8));
    let bus = Arc::new(Bus::default());
    let task = tokio::spawn(mirror(ledger.clone(), bus.clone(), &root).run());
    until(|| store.read() == CursorRead::Present(cursor("current", 8))).await;
    assert_eq!(
        *bus.accepted.lock().unwrap(),
        (2..=8)
            .map(|seq| entry(if seq <= 4 { "previous" } else { "current" }, seq))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        *ledger.queries.lock().unwrap(),
        [None, Some(6), Some(4), Some(2)]
    );
    assert_eq!(
        *ledger.subscriptions.lock().unwrap(),
        [cursor("previous", 1), cursor("current", 7)]
    );
    stop(task).await;
}

#[tokio::test]
async fn activity_lag_queries_from_last_successful_checkpoint() {
    let root = ScratchDir::new("activity-lag");
    let store = CursorStore::new(&root);
    store.write(&cursor("current", 2)).unwrap();
    let ledger = Ledger::new((1..=6).map(|seq| entry("current", seq)).collect());
    ledger.ends.lock().unwrap().push_back(FeedEnd::Lagged);
    let bus = Arc::new(Bus::default());
    let task = tokio::spawn(mirror(ledger.clone(), bus.clone(), &root).run());
    until(|| ledger.subscriptions.lock().unwrap().len() == 2).await;
    assert_eq!(
        *bus.accepted.lock().unwrap(),
        (3..=6).map(|seq| entry("current", seq)).collect::<Vec<_>>()
    );
    assert_eq!(
        *ledger.subscriptions.lock().unwrap(),
        [cursor("current", 2), cursor("current", 6)]
    );
    assert_eq!(store.read(), CursorRead::Present(cursor("current", 6)));
    assert_eq!(*ledger.queries.lock().unwrap(), [None, Some(5), Some(3)]);
    stop(task).await;
}

#[tokio::test]
async fn pruned_activity_entries_are_logged_as_lost_not_backfilled() {
    let root = ScratchDir::new("activity-pruned");
    let ledger = Ledger::new(vec![entry("previous", 4), entry("current", 6)]);
    let bus = Arc::new(Bus::default());
    let events = Arc::new(Events::default());
    let mut mirror = mirror(ledger.clone(), bus.clone(), &root);
    mirror.journal = events.clone();
    let head = mirror.recover(cursor("previous", 1)).await.unwrap();
    assert_eq!(head, cursor("current", 6));
    assert_eq!(
        *bus.accepted.lock().unwrap(),
        [entry("previous", 4), entry("current", 6)]
    );
    let lost = events.0.lock().unwrap();
    assert_eq!(lost.len(), 2);
    assert_eq!(lost[0].0, "ckbus.activity.entries_lost");
    assert_eq!(
        (
            lost[0].1["first_seq"].as_u64(),
            lost[0].1["last_seq"].as_u64()
        ),
        (Some(2), Some(3))
    );
    assert_eq!(
        (
            lost[1].1["first_seq"].as_u64(),
            lost[1].1["last_seq"].as_u64()
        ),
        (Some(5), Some(5))
    );
    drop(lost);
    // If every unseen entry has been pruned, an empty page still names the
    // high-water sequence and the whole missing interval must be reported.
    ledger.entries.lock().unwrap().clear();
    struct EmptyHead;
    #[async_trait]
    impl Source for EmptyHead {
        async fn query(&self, _: Option<u64>, _: u32) -> Result<ActivityQueryResponse, String> {
            Ok(ActivityQueryResponse {
                entries: vec![],
                cursor: None,
                head: cursor("current", 9),
            })
        }
        async fn subscribe(&self, _: ActivityCursor) -> Result<Box<dyn Feed>, FeedEnd> {
            unreachable!()
        }
    }
    mirror.source = Arc::new(EmptyHead);
    mirror.recover(head).await.unwrap();
    let lost = events.0.lock().unwrap();
    assert_eq!(
        (
            lost[2].1["first_seq"].as_u64(),
            lost[2].1["last_seq"].as_u64()
        ),
        (Some(7), Some(9))
    );
}

#[tokio::test]
async fn activity_checkpoint_write_failure_republishes_without_skipping() {
    let root = ScratchDir::new("activity-checkpoint-failed");
    let store = CursorStore::new(&root);
    store.write(&cursor("current", 1)).unwrap();
    std::fs::create_dir(root.join("activity_cursor.json.tmp")).unwrap();
    let bus = Arc::new(Bus::default());
    let mirror = mirror(Ledger::new(vec![]), bus.clone(), &root);
    assert!(mirror.publish(&entry("current", 2)).await.is_err());
    assert_eq!(store.read(), CursorRead::Present(cursor("current", 1)));
    std::fs::remove_dir(root.join("activity_cursor.json.tmp")).unwrap();
    mirror.publish(&entry("current", 2)).await.unwrap();
    assert_eq!(store.read(), CursorRead::Present(cursor("current", 2)));
    assert_eq!(
        *bus.accepted.lock().unwrap(),
        [entry("current", 2), entry("current", 2)]
    );
}

#[test]
fn activity_subject_identity_and_refusal_classification_match_wire_contract() {
    assert_eq!(
        subject("box_alpha").unwrap(),
        "ck.box_alpha.event.subc.activity.v1"
    );
    assert!(subject("bad.account").is_err());
    let body = serde_json::to_value(entry("original", 42)).unwrap();
    assert_eq!(body["daemon_incarnation"], "original");
    assert_eq!(body["seq"], 42);
    for code in [
        subc_client_rs::ACTIVITY_CURSOR_INCARNATION_MISMATCH,
        subc_client_rs::ACTIVITY_CURSOR_TOO_OLD,
    ] {
        assert_eq!(
            source::classify(Some(code), "detail".into()),
            FeedEnd::CursorRefused
        );
    }
    assert_eq!(
        source::classify(
            Some(subc_client_rs::ACTIVITY_SUBSCRIBER_LAGGED),
            "detail".into()
        ),
        FeedEnd::Lagged
    );
    assert_eq!(
        source::classify(Some("activity_read_forbidden"), "refused".into()),
        FeedEnd::Failed("refused".into())
    );
}

#[test]
fn activity_mirror_version_is_bumped_and_documented() {
    assert_eq!(env!("CARGO_PKG_VERSION"), "0.3.15");
    let changes = include_str!("../../CHANGELOG.md");
    assert!(changes.contains("## 0.3.15"));
    assert!(changes.contains("activity_cursor.json"));
}

struct StoredPlane {
    calls: Mutex<Vec<(String, Vec<u8>)>>,
    refuse: bool,
}
impl Default for StoredPlane {
    fn default() -> Self {
        Self {
            calls: Mutex::new(vec![]),
            refuse: true,
        }
    }
}
#[async_trait]
impl crate::bootstrap::plane::BoxPlane for StoredPlane {
    async fn publish_stored(
        &self,
        subject: &str,
        payload: Vec<u8>,
    ) -> Result<(), crate::bootstrap::plane::PlaneError> {
        self.calls.lock().unwrap().push((subject.into(), payload));
        if self.refuse {
            Err(crate::bootstrap::plane::PlaneError::new(
                "storage acknowledgement refused",
            ))
        } else {
            Ok(())
        }
    }
    async fn publish(
        &self,
        _: &str,
        _: Vec<u8>,
    ) -> Result<(), crate::bootstrap::plane::PlaneError> {
        Ok(())
    }
    async fn ensure_census(
        &self,
        _: &AccountNames,
    ) -> Result<(), crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn ensure_stream(
        &self,
        _: &cortexkit_bus_naming::StreamSpec,
    ) -> Result<(), crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn census_put(
        &self,
        _: &str,
        _: Vec<u8>,
    ) -> Result<(), crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn census_get(
        &self,
        _: &AccountNames,
        _: &str,
    ) -> Result<Option<crate::bootstrap::plane::CensusRecord>, crate::bootstrap::plane::PlaneError>
    {
        unreachable!()
    }
    async fn census_delete(
        &self,
        _: &AccountNames,
        _: &str,
        _: u64,
    ) -> Result<(), crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn create_durable(
        &self,
        _: &crate::bootstrap::plane::DurableConsumer,
    ) -> Result<(), crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn consumer_state(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<crate::bootstrap::plane::ConsumerState>, crate::bootstrap::plane::PlaneError>
    {
        unreachable!()
    }
    async fn delete_durable(
        &self,
        _: &str,
        _: &str,
    ) -> Result<bool, crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn purge_subject(
        &self,
        _: &str,
        _: &str,
    ) -> Result<u64, crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn consumer_names(
        &self,
        _: &str,
    ) -> Result<Vec<String>, crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
}
struct UnusedSystem;
#[async_trait]
impl crate::bootstrap::plane::SystemPlane for UnusedSystem {
    async fn list_accounts(&self) -> Result<Vec<String>, crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn lookup(&self, _: &str) -> Result<Option<String>, crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn update(&self, _: &str) -> Result<(), crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn kick(&self, _: &str, _: u64) -> Result<(), crate::bootstrap::plane::PlaneError> {
        unreachable!()
    }
    async fn watch_connections(
        &self,
        _: &str,
    ) -> Result<
        tokio::sync::mpsc::UnboundedReceiver<crate::bootstrap::plane::ConnectionEvent>,
        crate::bootstrap::plane::PlaneError,
    > {
        unreachable!()
    }
}

fn ready_plane(plane: Arc<dyn crate::bootstrap::plane::BoxPlane>) -> Arc<Ready> {
    Arc::new(Ready {
        account: crate::bootstrap::store::AccountRecord {
            machine_id: "machine".into(),
            acct: "box_alpha".into(),
            account_public: "unused".into(),
        },
        system_user: "unused".into(),
        box_user: "unused".into(),
        system: Arc::new(UnusedSystem),
        box_plane: plane,
        renewals: vec![],
    })
}

#[tokio::test]
async fn activity_bus_publish_preserves_identity_and_requires_storage_acknowledgement() {
    let plane = Arc::new(StoredPlane::default());
    let ready = ready_plane(plane.clone());
    let (_tx, ready) = watch::channel(Some(ready));
    let publisher = BusPublisher {
        ready,
        timeout: Duration::from_secs(1),
    };
    let result = publisher.publish(&entry("original", 42)).await;
    assert!(result
        .unwrap_err()
        .contains("storage acknowledgement refused"));
    let calls = plane.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ck.box_alpha.event.subc.activity.v1");
    let payload: ActivityEntry = serde_json::from_slice(&calls[0].1).unwrap();
    assert_eq!(payload, entry("original", 42));
}

pub(super) async fn module_frame(
    stream: &mut tokio::net::TcpStream,
    ty: subc_protocol::FrameType,
    corr: u64,
    body: Value,
) -> subc_protocol::Frame {
    let request = subc_protocol::Frame::build(
        ty,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(stream, &request).await.unwrap();
    timeout(Duration::from_secs(5), subc_transport::read_frame(stream))
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_daemon_activity_subscription_and_bus_outage_leave_ledger_independent() {
    use subc_protocol::FrameType;
    let root = ScratchDir::new("ckbus-activity-daemon");
    let connection_file = root.join("connection.json");
    let config = subc_daemon::bootstrap::BootstrapConfig::new(&connection_file, 0)
        .with_terminal_journal_path(root.join("terminals.jsonl"));
    let daemon = tokio::spawn(subc_daemon::bootstrap::run_with_config(config));
    until(|| connection_file.exists()).await;
    let info = subc_transport::read(&connection_file).unwrap();
    let endpoint = &info.endpoints[0];
    let mut module = tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .unwrap();
    subc_transport::authenticate_client(&mut module, &info, Duration::from_secs(5))
        .await
        .unwrap();
    let hello = module_frame(
        &mut module,
        FrameType::Hello,
        1,
        json!({
            "manifest": subc_protocol::manifest::ModuleManifest::builder("worker", "0.1.0").build(),
            "protocol_ver": subc_protocol::PROTOCOL_VERSION
        }),
    )
    .await;
    assert_eq!(hello.header.ty, FrameType::HelloAck);
    let record = |summary: &str| json!({"op": "activity.record", "kind": "worker.settled", "summary": summary});
    assert_eq!(
        module_frame(&mut module, FrameType::Request, 2, record("historical"))
            .await
            .header
            .ty,
        FrameType::Response
    );
    let source = Arc::new(source::DaemonSource::new(connection_file.clone()));
    let page = source.query(None, 10).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    let bus = Arc::new(Bus::default());
    let mirror = Mirror::new(source.clone(), bus.clone(), &root, Duration::from_secs(1));
    let baseline = mirror.load().await.unwrap();
    assert_eq!(baseline, page.head);
    let mut feed = source.subscribe(baseline.clone()).await.unwrap();
    *bus.fail.lock().unwrap() = Some(baseline.seq + 1);
    // Append while the bus refuses all publishes. The writer and query must
    // continue to work, and query-to-follow replay must preserve the identity.
    let response = module_frame(
        &mut module,
        FrameType::Request,
        3,
        record("live during outage"),
    )
    .await;
    assert_eq!(response.header.ty, FrameType::Response);
    let live = timeout(Duration::from_secs(5), feed.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(live.seq, baseline.seq + 1);
    assert_eq!(live.summary, "live during outage");
    drop(feed);
    *bus.fail.lock().unwrap() = Some(live.seq);
    let failed = timeout(Duration::from_secs(5), mirror.follow(baseline.clone()))
        .await
        .unwrap();
    assert!(
        matches!(failed, Err(FeedEnd::Failed(ref error)) if error == "injected publish failure")
    );
    assert_eq!(
        *bus.attempts.lock().unwrap(),
        [live.seq],
        "the publisher must actually fail before checking ledger independence"
    );
    assert!(bus.accepted.lock().unwrap().is_empty());
    assert_eq!(mirror.cursors.read(), CursorRead::Present(baseline.clone()));
    let page = source.query(None, 10).await.unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.entries[0], live);
    assert_eq!(page.entries[1].summary, "historical");
    assert_eq!(page.head.seq, live.seq);
    *bus.fail.lock().unwrap() = None;
    let plane = Arc::new(StoredPlane {
        calls: Mutex::new(vec![]),
        refuse: false,
    });
    let ready = ready_plane(plane.clone());
    let (_tx, ready) = watch::channel(Some(ready));
    let task = wire(connection_file, &root, ready, Duration::from_secs(1));
    until(|| {
        mirror.cursors.read() == CursorRead::Present(cursor(&live.daemon_incarnation, live.seq))
    })
    .await;
    let calls = plane.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ck.box_alpha.event.subc.activity.v1");
    assert_eq!(
        serde_json::from_slice::<ActivityEntry>(&calls[0].1).unwrap(),
        live
    );
    drop(calls);
    stop(task).await;
    drop(module);
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());
}

#[test]
fn supervised_main_starts_the_activity_mirror() {
    struct Wiring(bool);
    impl<'ast> syn::visit::Visit<'ast> for Wiring {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = call.func.as_ref() {
                let names: Vec<_> = path
                    .path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect();
                if names == ["activity_mirror", "wire"] {
                    self.0 = true;
                }
            }
            syn::visit::visit_expr_call(self, call);
        }
    }
    let source = syn::parse_file(include_str!("../main.rs")).unwrap();
    let main = source
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(fun) if fun.sig.ident == "main" => Some(fun),
            _ => None,
        })
        .unwrap();
    let mut wiring = Wiring(false);
    syn::visit::Visit::visit_block(&mut wiring, &main.block);
    assert!(
        wiring.0,
        "supervised startup must launch the activity mirror"
    );
}
