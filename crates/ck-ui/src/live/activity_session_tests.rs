//! The activity feed's session against a scripted ledger, on tokio's paused
//! clock: paging, resuming from the last head, backfilling after a refused
//! head, and the loaded cap around the person's row.

use std::collections::HashMap;

use serde_json::json;

use super::*;
use crate::live::activity::CAP;
use crate::live::schedule::Showing;
use crate::live::NoService;
use crate::model::{ActivityKey, ActivityLog, ActivityRowKind, Tab};

const INC: &str = "inc-1";
const BASE_MS: u64 = 1_700_000_000_000;

#[derive(Default)]
struct Inner {
    /// Every entry recorded, oldest first.
    entries: Vec<WireActivity>,
    /// A subscription from a head below this seq is refused as too old, as
    /// the daemon refuses one whose following entries are no longer all in
    /// its replay ring.
    ring_floor: u64,
    /// The next read of an open feed ends it as lagged.
    lag: bool,
    /// Entries recorded just before the next query is answered, as if they
    /// arrived while it was on its way: the reply has them, and so does an
    /// open feed.
    record_on_query: u64,
    /// The cursor of each `activity.query` sent.
    queries: Vec<Option<u64>>,
    /// The head each `activity.subscribe` was sent from.
    subscribes: Vec<Option<u64>>,
}

/// A ledger that answers like the daemon's: pages of [`activity::PAGE`]
/// newest first with an exclusive cursor, and feeds that replay what was
/// recorded after their head and then follow.
#[derive(Clone)]
struct ScriptedLedger {
    inner: Arc<Mutex<Inner>>,
    latest: Arc<watch::Sender<u64>>,
}

fn entry(seq: u64) -> WireActivity {
    serde_json::from_value(json!({
        "seq": seq, "at_ms": BASE_MS + seq * 1_000, "daemon_incarnation": INC,
        "module": "broca", "kind": "broca.run_ended", "summary": "run ended", "details": {},
        "actor": { "principal": { "kind": "direct" } }
    }))
    .unwrap()
}

fn key(seq: u64) -> ActivityKey {
    ActivityKey::Entry {
        incarnation: INC.into(),
        seq,
    }
}

impl ScriptedLedger {
    fn new() -> ScriptedLedger {
        ScriptedLedger {
            inner: Arc::default(),
            latest: Arc::new(watch::Sender::new(0)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    /// Records `n` more entries.
    fn record(&self, n: u64) {
        let top = {
            let mut inner = self.lock();
            let next = inner.entries.last().map_or(1, |e| e.seq + 1);
            inner.entries.extend((next..next + n).map(entry));
            next + n - 1
        };
        self.latest.send_replace(top);
    }

    fn refuse_heads_below(&self, seq: u64) {
        self.lock().ring_floor = seq;
    }

    /// Ends the open feed as lagged.
    fn lag(&self) {
        self.lock().lag = true;
        self.latest.send_modify(|_| {});
    }

    fn record_on_next_query(&self, n: u64) {
        self.lock().record_on_query = n;
    }

    fn queries(&self) -> Vec<Option<u64>> {
        self.lock().queries.clone()
    }

    fn subscribes(&self) -> Vec<Option<u64>> {
        self.lock().subscribes.clone()
    }
}

impl LedgerSource for ScriptedLedger {
    fn query(&self, cursor: Option<u64>) -> BoxFuture<'_, Result<ActivityQueryResponse, Fault>> {
        let arriving = std::mem::take(&mut self.lock().record_on_query);
        if arriving > 0 {
            self.record(arriving);
        }
        let reply = {
            let mut inner = self.lock();
            inner.queries.push(cursor);
            let below: Vec<&WireActivity> = inner
                .entries
                .iter()
                .rev()
                .filter(|e| cursor.is_none_or(|c| e.seq < c))
                .collect();
            let page: Vec<WireActivity> = below
                .iter()
                .take(activity::PAGE as usize)
                .map(|e| (*e).clone())
                .collect();
            let more = below.len() > page.len();
            ActivityQueryResponse {
                cursor: if more {
                    page.last().map(|e| e.seq)
                } else {
                    None
                },
                head: ActivityCursor {
                    daemon_incarnation: INC.into(),
                    seq: inner.entries.last().map_or(0, |e| e.seq),
                },
                entries: page,
            }
        };
        Box::pin(async move { Ok(reply) })
    }

    fn subscribe(
        &self,
        since: Option<ActivityCursor>,
    ) -> BoxFuture<'_, Result<Box<dyn LedgerFeed>, FeedEnd>> {
        let refused = {
            let mut inner = self.lock();
            inner.subscribes.push(since.as_ref().map(|c| c.seq));
            since.as_ref().is_some_and(|c| c.seq < inner.ring_floor)
        };
        let feed = ScriptedFeed {
            ledger: self.clone(),
            pos: since.map_or_else(|| *self.latest.borrow(), |c| c.seq),
            rx: self.latest.subscribe(),
        };
        Box::pin(async move {
            if refused {
                Err(FeedEnd::CursorRefused)
            } else {
                Ok(Box::new(feed) as Box<dyn LedgerFeed>)
            }
        })
    }
}

struct ScriptedFeed {
    ledger: ScriptedLedger,
    /// The seq of the last entry this feed delivered.
    pos: u64,
    rx: watch::Receiver<u64>,
}

impl LedgerFeed for ScriptedFeed {
    fn next(&mut self) -> BoxFuture<'_, Result<WireActivity, FeedEnd>> {
        Box::pin(async move {
            loop {
                let next = {
                    let mut inner = self.ledger.lock();
                    if std::mem::take(&mut inner.lag) {
                        return Err(FeedEnd::Lagged);
                    }
                    inner.entries.iter().find(|e| e.seq > self.pos).cloned()
                };
                if let Some(e) = next {
                    self.pos = e.seq;
                    return Ok(e);
                }
                if self.rx.changed().await.is_err() {
                    return Err(FeedEnd::Ended);
                }
            }
        })
    }
}

/// The activity session and the request handler, run against a scripted
/// ledger.
struct Run {
    shared: Arc<Shared>,
    requests: mpsc::UnboundedSender<Request>,
    tasks: Vec<JoinHandle<()>>,
}

impl Run {
    fn start(ledger: &ScriptedLedger, showing: Showing) -> Run {
        let state = State::empty("This computer");
        let (tx, _rx) = watch::channel(assemble(&state, now()));
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            tx,
            service: Arc::new(NoService),
            conn: Mutex::new(None),
            provenance_due: Notify::new(),
            log_due: Notify::new(),
            probe_due: Notify::new(),
            activity_due: Notify::new(),
            demand: watch::Sender::new(schedule::wanted(&showing)),
            identity_ready: Notify::new(),
            raw: Arc::default(),
        });
        let gate = shared.gate();
        let source: Arc<dyn LedgerSource> = Arc::new(ledger.clone());
        let sh = Arc::clone(&shared);
        let session = schedule::gated_session(&gate, Source::Activity, move || {
            activity_session(Arc::clone(&sh), Arc::clone(&source))
        });
        let (requests, rx) = mpsc::unbounded_channel();
        let handler = tokio::spawn(handle_requests(Arc::clone(&shared), rx));
        Run {
            shared,
            requests,
            tasks: vec![session, handler],
        }
    }

    fn send(&self, r: Request) {
        self.requests.send(r).unwrap();
    }

    fn show(&self, tab: Tab) {
        self.send(Request::Show(Showing::tab(tab)));
    }

    fn log(&self) -> ActivityLog {
        self.shared.read(|s| s.activity_log(&HashMap::new()))
    }

    /// The seq of every loaded row, top to bottom.
    fn seqs(&self) -> Vec<u64> {
        self.log().rows.iter().filter_map(|r| r.seq).collect()
    }

    /// The view reaches the oldest loaded row and asks for the next page.
    async fn page_down(&self) {
        let oldest = self.log().rows.last().map(|r| r.key.clone());
        self.send(Request::ActivityView {
            anchor: oldest,
            older: true,
        });
        settle().await;
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Lets every task run until it waits on something that hasn't happened.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

fn newest_first(range: std::ops::RangeInclusive<u64>) -> Vec<u64> {
    range.rev().collect()
}

#[tokio::test(start_paused = true)]
async fn the_first_page_holds_100_and_older_pages_use_the_previous_replys_cursor() {
    let ledger = ScriptedLedger::new();
    ledger.record(250);
    let run = Run::start(&ledger, Showing::tab(Tab::Activity));
    settle().await;
    assert_eq!(ledger.queries(), [None]);
    assert_eq!(ledger.subscribes(), [Some(250)], "from the page's head");
    assert_eq!(run.seqs(), newest_first(151..=250));
    assert!(!run.log().history_exhausted);

    for _ in 0..3 {
        run.page_down().await;
    }
    // Each page below the last reply's cursor; the third view request finds
    // history exhausted and sends nothing.
    assert_eq!(ledger.queries(), [None, Some(151), Some(51)]);
    assert_eq!(run.seqs(), newest_first(1..=250));
    assert!(run.log().history_exhausted);
}

#[tokio::test(start_paused = true)]
async fn entries_recorded_while_usage_shows_appear_in_order_on_returning_to_activity() {
    let ledger = ScriptedLedger::new();
    ledger.record(120);
    let run = Run::start(&ledger, Showing::tab(Tab::Activity));
    settle().await;
    ledger.record(5);
    settle().await;
    assert_eq!(run.seqs(), newest_first(21..=125));
    assert!(run.shared.read(|s| s.ledger.open));

    run.show(Tab::Usage);
    settle().await;
    assert!(
        !run.shared.read(|s| s.ledger.open),
        "the feed is closed while Usage shows"
    );
    ledger.record(40);
    settle().await;
    assert_eq!(run.seqs(), newest_first(21..=125), "nothing read on Usage");

    run.show(Tab::Activity);
    settle().await;
    assert_eq!(ledger.queries(), [None], "no re-query of the newest 100");
    assert_eq!(
        ledger.subscribes(),
        [Some(120), Some(125)],
        "from the last head"
    );
    assert_eq!(run.seqs(), newest_first(21..=165));
    assert!(run.shared.read(|s| s.ledger.open));
}

#[tokio::test(start_paused = true)]
async fn a_refused_resubscribe_backfills_350_entries_once_in_seq_order_with_no_gap() {
    let ledger = ScriptedLedger::new();
    ledger.record(100);
    let run = Run::start(&ledger, Showing::tab(Tab::Activity));
    settle().await;
    run.show(Tab::Usage);
    settle().await;
    ledger.record(350);
    // The feed's head (100) is below the replay ring's floor, so the
    // resubscribe is refused as too old.
    ledger.refuse_heads_below(200);
    run.show(Tab::Activity);
    settle().await;

    assert_eq!(
        ledger.subscribes(),
        [Some(100), Some(100), Some(450)],
        "refused, then from the newest page's head"
    );
    // The newest page, then each reply's cursor, until a page reaches an
    // entry already loaded.
    assert_eq!(
        ledger.queries(),
        [None, None, Some(351), Some(251), Some(151)]
    );
    let log = run.log();
    assert_eq!(run.seqs(), newest_first(1..=450));
    assert!(log.rows.iter().all(|r| r.row == ActivityRowKind::Entry));
}

#[tokio::test(start_paused = true)]
async fn a_closed_subscription_resubscribes_from_the_last_head() {
    let ledger = ScriptedLedger::new();
    ledger.record(100);
    let run = Run::start(&ledger, Showing::tab(Tab::Activity));
    settle().await;
    ledger.record(3);
    settle().await;
    ledger.lag();
    settle().await;
    ledger.record(2);
    settle().await;
    assert_eq!(ledger.subscribes(), [Some(100), Some(103)]);
    assert_eq!(ledger.queries(), [None]);
    assert_eq!(run.seqs(), newest_first(1..=105));
}

#[tokio::test(start_paused = true)]
async fn with_3000_entries_the_selected_entry_2500_stays_loaded_and_selected() {
    let ledger = ScriptedLedger::new();
    ledger.record(3_000);
    let run = Run::start(&ledger, Showing::tab(Tab::Activity));
    settle().await;
    // Entry 2,500 counting from the newest.
    let selected = key(501);
    for _ in 0..30 {
        if run.log().position(&selected).is_some() {
            break;
        }
        run.page_down().await;
    }
    assert!(run.log().position(&selected).is_some(), "paged down to it");
    run.send(Request::ActivityView {
        anchor: Some(selected.clone()),
        older: false,
    });
    settle().await;

    // Live entries arrive while the person reads, and more older pages load
    // below the selection.
    ledger.record(300);
    settle().await;
    for _ in 0..3 {
        run.send(Request::ActivityView {
            anchor: Some(selected.clone()),
            older: true,
        });
        settle().await;
    }

    let log = run.log();
    let at = log
        .position(&selected)
        .expect("entry 2,500 is still loaded");
    assert_eq!(log.rows[at].seq, Some(501));
    assert_eq!(log.counted(), CAP);
    assert!(
        log.newer_unloaded,
        "the newest were dropped, not the selection"
    );
    let seqs = run.seqs();
    assert!(
        seqs.windows(2).all(|w| w[0] == w[1] + 1),
        "one contiguous range"
    );
}

#[tokio::test(start_paused = true)]
async fn dropped_newest_entries_are_read_again_when_the_view_returns_to_the_top() {
    let ledger = ScriptedLedger::new();
    ledger.record(2_300);
    let run = Run::start(&ledger, Showing::tab(Tab::Activity));
    settle().await;
    for _ in 0..30 {
        if run.log().history_exhausted {
            break;
        }
        run.page_down().await;
    }
    let log = run.log();
    assert!(log.history_exhausted, "paged down to the start of history");
    assert!(log.newer_unloaded);
    assert_eq!(run.seqs(), newest_first(1..=2_000));
    // Recorded while the newest are dropped: read with them later.
    ledger.record(5);
    settle().await;
    assert_eq!(run.seqs(), newest_first(1..=2_000));

    // The person goes back up to the first loaded row. Two more entries are
    // recorded while the newest page is on its way, so the page and the
    // open feed both bring them.
    let top = log.rows[0].key.clone();
    ledger.record_on_next_query(2);
    run.send(Request::ActivityView {
        anchor: Some(top.clone()),
        older: false,
    });
    settle().await;
    let log = run.log();
    assert!(!log.newer_unloaded);
    assert_eq!(run.seqs(), newest_first(308..=2_307), "each entry once");
    assert!(log.position(&top).is_some());
    assert_eq!(ledger.subscribes(), [Some(2_300)], "the feed stayed open");
}
