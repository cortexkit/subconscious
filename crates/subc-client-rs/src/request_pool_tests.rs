//! Handler-slot pools, driven through the real dispatch path: frames go in
//! through `handle_frame`, test handlers really block, and the pools are the
//! production semaphores, never stand-ins for them.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::json;
use tokio::{
    sync::Semaphore,
    time::{advance, timeout},
};
use tracing::{
    field::{Field, Visit},
    span,
};

use super::*;

const ROUTE: u16 = 7;
const LONG_OP: &str = "research.run";

/// A body ending in `hold` blocks its handler until the test releases it;
/// any other body is answered at once. Every answer echoes the body.
struct PoolHandler {
    held: AtomicUsize,
    release: Semaphore,
    pools: RequestPools,
    report: HealthReport,
}

#[async_trait]
impl ModuleHandler for PoolHandler {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if body.ends_with(b"hold") {
            self.held.fetch_add(1, Ordering::SeqCst);
            self.release.acquire().await.unwrap().forget();
        }
        HandlerOutcome::Response(body)
    }

    async fn health(&self) -> HealthReport {
        self.report.clone()
    }

    fn request_pools(&self) -> RequestPools {
        self.pools.clone()
    }
}

/// Bodies starting with `long` belong to the long-held pool and carry an
/// operation label; every other body uses the default pool with none.
fn classify(body: &[u8]) -> RequestClass {
    if body.starts_with(b"long") {
        RequestClass::long_held().with_op(LONG_OP)
    } else {
        RequestClass::default_pool()
    }
}

fn two_pools(long_held_limit: usize) -> RequestPools {
    RequestPools::new().with_long_held(long_held_limit, classify)
}

fn pool_handler(pools: RequestPools, report: HealthReport) -> PoolHandler {
    PoolHandler {
        held: AtomicUsize::new(0),
        release: Semaphore::new(0),
        pools,
        report,
    }
}

fn snapshot(
    pool: RequestPool,
    in_use: usize,
    limit: usize,
    waiting: usize,
    oldest_wait: Option<Duration>,
) -> RequestPoolSnapshot {
    RequestPoolSnapshot {
        pool,
        in_use,
        limit,
        waiting,
        oldest_wait,
    }
}

/// Let every task that is ready run, without parking the runtime. Parking
/// with the clock paused would let tokio jump the clock to the next timer.
async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

struct PoolHarness {
    tx: mpsc::Sender<Frame>,
    rx: mpsc::Receiver<Frame>,
    handler: Arc<PoolHandler>,
    dispatcher: RequestDispatcher,
    module_handle: ModuleHandle,
}

impl PoolHarness {
    fn new(pools: RequestPools) -> Self {
        Self::with_report(pools, HealthReport::ok())
    }

    fn with_report(pools: RequestPools, report: HealthReport) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let (control_tx, _control_rx) = mpsc::channel(4);
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: Vec::new(),
            subc_capabilities: Vec::new(),
            storage: None,
            machine_id: None,
        };
        let module_handle = ModuleHandle::new(
            &ack,
            control_tx,
            1,
            CancellationToken::new(),
            tokio::runtime::Handle::current(),
        );
        module_handle
            .install_route(RouteHandle::new(ROUTE, 1, 1))
            .unwrap();
        let handler = Arc::new(pool_handler(pools, report));
        // The same wiring as serving: the handle takes the handler's pools and
        // the dispatcher uses the handle's.
        module_handle.install_dispatch_pools(handler.request_pools());
        let dispatcher = RequestDispatcher::with_pools(module_handle.dispatch_pools());
        Self {
            tx,
            rx,
            handler,
            dispatcher,
            module_handle,
        }
    }

    async fn send(&self, corr: u64, body: &[u8]) {
        let frame = Frame::build(
            FrameType::Request,
            data_flags(),
            ROUTE,
            1,
            corr,
            body.to_vec(),
        )
        .unwrap();
        handle_frame(
            frame,
            &self.tx,
            Arc::clone(&self.handler),
            self.dispatcher.clone(),
            self.module_handle.clone(),
        )
        .await
        .unwrap();
    }

    async fn cancel(&self, corr: u64) {
        let frame =
            Frame::build(FrameType::Cancel, data_flags(), ROUTE, 1, corr, Vec::new()).unwrap();
        handle_frame(
            frame,
            &self.tx,
            Arc::clone(&self.handler),
            self.dispatcher.clone(),
            self.module_handle.clone(),
        )
        .await
        .unwrap();
    }

    /// Wait until `count` handlers are holding their requests. Bounded by a
    /// number of yields rather than a timeout: with the clock paused, a
    /// timeout never fires while the loop keeps yielding, so a broken pool
    /// would hang the test instead of failing it.
    async fn wait_for_held(&self, count: usize) {
        for _ in 0..10_000 {
            if self.handler.held.load(Ordering::SeqCst) == count {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "expected {count} held handlers, have {}",
            self.handler.held.load(Ordering::SeqCst)
        );
    }

    async fn next_frame(&mut self, what: &str) -> Frame {
        timeout(Duration::from_secs(1), self.rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{what}"))
            .unwrap()
    }

    fn pool(&self, pool: RequestPool) -> RequestPoolSnapshot {
        self.module_handle
            .request_pool_snapshots()
            .into_iter()
            .find(|snapshot| snapshot.pool == pool)
            .unwrap()
    }

    /// Release `count` held handlers and return the correlation ids of the
    /// `count` responses that follow.
    async fn release(&mut self, count: usize) -> Vec<u64> {
        self.handler.release.add_permits(count);
        let mut corrs = Vec::new();
        for _ in 0..count {
            let frame = self.next_frame("a released handler must answer").await;
            assert_eq!(frame.header.ty, FrameType::Response);
            corrs.push(frame.header.corr);
        }
        corrs.sort_unstable();
        corrs
    }

    async fn health(&mut self, corr: u64) -> HealthReport {
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).unwrap(),
        )
        .unwrap();
        handle_frame(
            request,
            &self.tx,
            Arc::clone(&self.handler),
            self.dispatcher.clone(),
            self.module_handle.clone(),
        )
        .await
        .unwrap();
        let frame = self
            .next_frame("health must answer while pools are full")
            .await;
        assert_eq!((frame.header.channel, frame.header.corr), (0, corr));
        serde_json::from_slice::<ModuleControlResponse>(&frame.body)
            .unwrap()
            .health_report()
            .unwrap()
    }
}

#[tokio::test]
async fn a_default_pool_request_completes_while_the_long_held_pool_is_full() {
    let mut harness = PoolHarness::new(two_pools(2));
    harness.send(1, b"long hold").await;
    harness.send(2, b"long hold").await;
    harness.wait_for_held(2).await;
    harness.send(3, b"long hold").await;
    settle().await;
    assert_eq!(harness.handler.held.load(Ordering::SeqCst), 2);
    let long_held = harness.pool(RequestPool::LongHeld);
    assert_eq!(
        (long_held.in_use, long_held.limit, long_held.waiting),
        (2, 2, 1)
    );
    assert_eq!(harness.pool(RequestPool::Default).in_use, 0);

    harness.send(4, b"quick").await;
    let answer = harness
        .next_frame("a default-pool request must not wait behind a full long-held pool")
        .await;
    assert_eq!(
        (answer.header.ty, answer.header.corr, answer.body.as_slice()),
        (FrameType::Response, 4, &b"quick"[..])
    );

    assert_eq!(harness.release(3).await, vec![1, 2, 3]);
}

#[tokio::test]
async fn a_long_held_request_completes_while_the_default_pool_is_full() {
    let mut harness = PoolHarness::new(two_pools(2));
    for corr in 1..=HANDLER_TASK_CAPACITY as u64 {
        harness.send(corr, b"hold").await;
    }
    harness.wait_for_held(HANDLER_TASK_CAPACITY).await;
    harness.send(65, b"hold").await;
    settle().await;
    let default = harness.pool(RequestPool::Default);
    assert_eq!(
        (default.in_use, default.waiting),
        (HANDLER_TASK_CAPACITY, 1)
    );

    harness.send(100, b"long quick").await;
    let answer = harness
        .next_frame("a long-held request must not wait behind a full default pool")
        .await;
    assert_eq!(
        (answer.header.ty, answer.header.corr, answer.body.as_slice()),
        (FrameType::Response, 100, &b"long quick"[..])
    );

    let released = harness.release(HANDLER_TASK_CAPACITY + 1).await;
    assert_eq!(released, (1..=65).collect::<Vec<u64>>());
}

/// Records every WARN event's fields, on the thread that installs it. A
/// current-thread test runtime runs every spawned task on that thread.
#[derive(Clone, Default)]
struct WarnCapture(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

impl WarnCapture {
    fn all(&self) -> Vec<BTreeMap<String, String>> {
        self.0.lock().unwrap().clone()
    }

    fn slot_waits(&self) -> Vec<BTreeMap<String, String>> {
        self.all()
            .into_iter()
            .filter(|event| {
                event
                    .get("message")
                    .is_some_and(|message| message.contains("waiting for a handler slot"))
            })
            .collect()
    }
}

struct FieldRecorder<'a>(&'a mut BTreeMap<String, String>);

impl Visit for FieldRecorder<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl tracing::Subscriber for WarnCapture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = BTreeMap::new();
        event.record(&mut FieldRecorder(&mut fields));
        self.0.lock().unwrap().push(fields);
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

/// The clock is paused from the start (`start_paused`), so every timer in
/// this test runs on the same virtual clock and only `advance` moves it.
/// The warning is absent at 1.999 s, present once at 2 s while the request
/// still waits, and still once a minute later.
#[tokio::test(start_paused = true)]
async fn a_request_waiting_two_seconds_for_a_slot_is_warned_about_once_while_it_waits() {
    let capture = WarnCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let mut harness = PoolHarness::new(two_pools(1));
    harness.send(1, b"long hold").await;
    harness.wait_for_held(1).await;
    // The warning names the request; it must never repeat its body.
    harness.send(2, b"long private-body-text hold").await;
    settle().await;
    assert_eq!(harness.pool(RequestPool::LongHeld).waiting, 1);

    advance(Duration::from_millis(1999)).await;
    settle().await;
    assert!(
        capture.slot_waits().is_empty(),
        "no warning before two seconds: {:?}",
        capture.all()
    );

    advance(Duration::from_millis(1)).await;
    settle().await;
    let warnings = capture.slot_waits();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let warning = &warnings[0];
    for (field, expected) in [
        ("op", LONG_OP),
        ("pool", "long_held"),
        ("in_use", "1"),
        ("limit", "1"),
        ("channel", "7"),
        ("corr", "2"),
    ] {
        assert_eq!(
            warning.get(field).map(String::as_str),
            Some(expected),
            "field {field} of {warning:?}"
        );
    }
    // Logged while the request was still waiting, not after it got a slot.
    assert_eq!(harness.pool(RequestPool::LongHeld).waiting, 1);
    assert_eq!(harness.handler.held.load(Ordering::SeqCst), 1);

    advance(Duration::from_secs(60)).await;
    settle().await;
    assert_eq!(capture.slot_waits().len(), 1, "{:?}", capture.all());

    assert_eq!(harness.release(2).await, vec![1, 2]);
    assert_eq!(capture.slot_waits().len(), 1, "{:?}", capture.all());
    assert!(
        !format!("{:?}", capture.all()).contains("private-body-text"),
        "a warning repeated the request body"
    );
}

#[tokio::test(start_paused = true)]
async fn a_request_without_an_op_label_is_warned_about_as_having_none() {
    let capture = WarnCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let mut harness = PoolHarness::new(two_pools(1));
    for corr in 1..=HANDLER_TASK_CAPACITY as u64 {
        harness.send(corr, b"hold").await;
    }
    harness.wait_for_held(HANDLER_TASK_CAPACITY).await;
    harness.send(65, b"hold").await;
    settle().await;
    advance(Duration::from_secs(2)).await;
    settle().await;
    let warnings = capture.slot_waits();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].get("op").map(String::as_str), Some("<none>"));
    assert_eq!(warnings[0].get("pool").map(String::as_str), Some("default"));
    assert_eq!(warnings[0].get("in_use").map(String::as_str), Some("64"));
    assert_eq!(warnings[0].get("limit").map(String::as_str), Some("64"));
    harness.release(HANDLER_TASK_CAPACITY + 1).await;
}

#[tokio::test]
async fn a_long_held_request_cancelled_while_waiting_is_answered_without_a_slot() {
    let mut harness = PoolHarness::new(two_pools(1));
    harness.send(1, b"long hold").await;
    harness.wait_for_held(1).await;
    harness.send(2, b"long hold").await;
    settle().await;
    assert_eq!(harness.pool(RequestPool::LongHeld).waiting, 1);

    harness.cancel(2).await;
    let answer = harness
        .next_frame("a request cancelled while waiting must be answered at once")
        .await;
    assert_eq!(
        (answer.header.ty, answer.header.corr),
        (FrameType::Error, 2)
    );
    assert_eq!(
        serde_json::from_slice::<ErrorBody>(&answer.body)
            .unwrap()
            .code,
        "cancelled"
    );
    settle().await;
    assert_eq!(
        harness.pool(RequestPool::LongHeld),
        snapshot(RequestPool::LongHeld, 1, 1, 0, None)
    );
    assert_eq!(harness.release(1).await, vec![1]);
    assert_eq!(harness.handler.held.load(Ordering::SeqCst), 1);
}

/// When the connection ends, a request still waiting for a long-held slot
/// ends without any reply, never runs its handler, and leaves the pool's
/// waiting count at 0, exactly as a waiter in the default pool does.
///
/// The test runs `RequestDispatcher::cancel_all`, the step the serve loop
/// takes when the connection ends, and then lets every task run before the
/// route is torn down. In a full serve the handle clears its routes right
/// after `cancel_all`, which silently drops a late reply on a current-thread
/// runtime and races with it on a multi-threaded one; with the route still
/// live here, any reply the waiter tries to send reaches the egress channel
/// and fails the test.
#[tokio::test]
async fn a_long_held_request_waiting_when_the_connection_ends_gets_no_reply() {
    let mut harness = PoolHarness::new(two_pools(1));
    harness.send(1, b"long hold").await;
    harness.wait_for_held(1).await;
    harness.send(2, b"long hold").await;
    settle().await;
    assert_eq!(harness.pool(RequestPool::LongHeld).waiting, 1);

    harness.dispatcher.cancel_all();
    settle().await;

    let replies = std::iter::from_fn(|| harness.rx.try_recv().ok())
        .map(|frame| (frame.header.ty, frame.header.corr))
        .collect::<Vec<_>>();
    assert!(
        replies.is_empty(),
        "a request waiting when the connection ended must get no reply, got {replies:?}"
    );
    assert_eq!(harness.handler.held.load(Ordering::SeqCst), 1);
    assert_eq!(harness.pool(RequestPool::LongHeld).waiting, 0);
}

#[tokio::test(start_paused = true)]
async fn pool_snapshots_and_health_report_each_pool_under_load() {
    let own_report = HealthReport {
        status: HealthStatus::Ok,
        detail: Some("module gauges healthy".to_string()),
        metrics: Some(json!({"runs": 2})),
    };
    let mut harness = PoolHarness::with_report(two_pools(2), own_report.clone());
    assert_eq!(
        harness.module_handle.request_pool_snapshots(),
        vec![
            snapshot(RequestPool::Default, 0, HANDLER_TASK_CAPACITY, 0, None),
            snapshot(RequestPool::LongHeld, 0, 2, 0, None),
        ]
    );
    for corr in 1..=HANDLER_TASK_CAPACITY as u64 {
        harness.send(corr, b"hold").await;
    }
    harness.send(101, b"long hold").await;
    harness.send(102, b"long hold").await;
    harness.wait_for_held(HANDLER_TASK_CAPACITY + 2).await;
    harness.send(65, b"hold").await;
    harness.send(66, b"hold").await;
    harness.send(103, b"long hold").await;
    settle().await;

    advance(Duration::from_millis(5000)).await;
    settle().await;
    assert_eq!(
        harness.module_handle.request_pool_snapshots(),
        vec![
            snapshot(
                RequestPool::Default,
                HANDLER_TASK_CAPACITY,
                HANDLER_TASK_CAPACITY,
                2,
                Some(Duration::from_secs(5))
            ),
            snapshot(RequestPool::LongHeld, 2, 2, 1, Some(Duration::from_secs(5))),
        ]
    );
    let report = harness.health(900).await;
    assert_eq!(report.status, HealthStatus::Degraded);
    assert_eq!(report.metrics, own_report.metrics);
    assert_eq!(
        report.detail.as_deref(),
        Some(
            "request dispatch saturated in the default pool: 64/64 in use, oldest waiting 5.0 s; \
             request dispatch saturated in the long_held pool: 2/2 in use, oldest waiting 5.0 s; \
             module gauges healthy"
        )
    );

    let released = harness.release(HANDLER_TASK_CAPACITY + 5).await;
    assert_eq!(released.len(), HANDLER_TASK_CAPACITY + 5);
    assert_eq!(
        harness.module_handle.request_pool_snapshots(),
        vec![
            snapshot(RequestPool::Default, 0, HANDLER_TASK_CAPACITY, 0, None),
            snapshot(RequestPool::LongHeld, 0, 2, 0, None),
        ]
    );
    assert_eq!(harness.health(901).await, own_report);
}

/// With no classifier, a body a classifier would call long-held still uses
/// the one default pool, and health keeps its single-pool wording.
#[tokio::test(start_paused = true)]
async fn without_a_classifier_every_request_uses_the_default_pool_as_before() {
    let own_report = HealthReport {
        status: HealthStatus::Ok,
        detail: Some("module gauges healthy".to_string()),
        metrics: None,
    };
    let mut harness = PoolHarness::with_report(RequestPools::new(), own_report.clone());
    for corr in 1..=HANDLER_TASK_CAPACITY as u64 {
        harness.send(corr, b"long hold").await;
    }
    harness.wait_for_held(HANDLER_TASK_CAPACITY).await;
    harness.send(65, b"long hold").await;
    settle().await;
    advance(Duration::from_millis(7300)).await;
    settle().await;
    assert_eq!(
        harness.module_handle.request_pool_snapshots(),
        vec![snapshot(
            RequestPool::Default,
            HANDLER_TASK_CAPACITY,
            HANDLER_TASK_CAPACITY,
            1,
            Some(Duration::from_millis(7300))
        )]
    );
    assert_eq!(
        harness.health(900).await.detail.as_deref(),
        Some(
            "request dispatch saturated: 64/64 in use, oldest waiting 7.3 s; module gauges healthy"
        )
    );
    let released = harness.release(HANDLER_TASK_CAPACITY + 1).await;
    assert_eq!(released, (1..=65).collect::<Vec<u64>>());
    assert_eq!(harness.health(901).await, own_report);
}

#[test]
fn a_panicking_classifier_puts_the_request_in_the_default_pool() {
    let pools = DispatchPools::new(RequestPools::new().with_long_held(4, |_| panic!("classifier")));
    let (pool, op) = pools.classify(b"anything");
    assert_eq!((pool.pool, op), (RequestPool::Default, None));
}

#[test]
fn a_long_held_limit_of_zero_is_raised_to_one() {
    let pools = RequestPools::new().with_long_held(0, classify);
    assert_eq!(pools.long_held_limit(), Some(1));
    assert_eq!(RequestPools::new().long_held_limit(), None);
}

/// The serve path itself: the handle a served module gets reports the pools
/// its handler declared before any frame is read, and a request the
/// classifier calls long-held occupies a long-held slot on the live
/// connection.
#[tokio::test]
async fn a_served_module_dispatches_through_the_pools_its_handler_declares() {
    let mut served = stand_in::serve_against_stand_in_with_ops(
        |_| pool_handler(two_pools(3), HealthReport::ok()),
        &[],
    )
    .await;
    assert_eq!(
        served.handle.request_pool_snapshots(),
        vec![
            snapshot(RequestPool::Default, 0, HANDLER_TASK_CAPACITY, 0, None),
            snapshot(RequestPool::LongHeld, 0, 3, 0, None),
        ]
    );

    let bind = serde_json::to_vec(&ModuleControlRequest::RouteBind {
        route_channel: ROUTE,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "pool-test".to_string(),
        },
        identity: BindIdentity::new(
            PathBuf::from("/tmp/project"),
            "test".to_string(),
            "pools".to_string(),
        ),
        principal: None,
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope: None,
        call_mode: None,
    })
    .unwrap();
    write_frame(
        &mut served.daemon,
        &Frame::build(FrameType::Request, control_flags(), 0, 0, 2, bind).unwrap(),
    )
    .await
    .unwrap();
    loop {
        let frame = read_frame(&mut served.daemon).await.unwrap().unwrap();
        if frame.header.channel == 0 && frame.header.corr == 2 {
            assert_eq!(
                frame.header.ty,
                FrameType::Response,
                "route.bind must be acked"
            );
            break;
        }
    }
    write_frame(
        &mut served.daemon,
        &Frame::build(
            FrameType::Request,
            data_flags(),
            ROUTE,
            1,
            10,
            b"long hold".to_vec(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(2), async {
        while served
            .handle
            .request_pool_snapshots()
            .iter()
            .find(|snapshot| snapshot.pool == RequestPool::LongHeld)
            .unwrap()
            .in_use
            != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the long request must hold a long-held slot");
    assert_eq!(served.handle.request_pool_snapshots()[0].in_use, 0);

    let quiet = stand_in::serve_against_stand_in_with_ops(
        |_| pool_handler(RequestPools::new(), HealthReport::ok()),
        &[],
    )
    .await;
    assert_eq!(
        quiet.handle.request_pool_snapshots(),
        vec![snapshot(
            RequestPool::Default,
            0,
            HANDLER_TASK_CAPACITY,
            0,
            None
        )]
    );
}
