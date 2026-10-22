//! The activity ledger as the live layer holds it for the Activity screen
//! and Home: the entries loaded so far, how to load more, where the feed
//! resumes from, and the rows they make once merged with process exits.
//!
//! The ledger is read newest first, a page at a time, and then followed
//! with a subscription from the head the first page reported. Loaded
//! entries are one contiguous run of seqs, except where the daemon no
//! longer keeps the entries between two runs; a label row marks each such
//! place. At most [`CAP`] rows (entries and exits; labels don't count)
//! stay loaded: past that, the rows farthest from the view are dropped, so
//! the person's row always stays.
//!
//! Merging is by each entry's identity, (daemon incarnation, seq), so an
//! entry read twice (a page that overlaps the feed, a replay after a
//! resubscribe) shows once. Seq is one order across daemon incarnations:
//! a restarted daemon numbers on from the highest seq it kept.

use std::collections::{BTreeSet, HashMap, HashSet};

use futures_util::future::BoxFuture;
use subc_control::{
    ActivityCursor, ActivityEntry as WireActivity, ActivityQueryResponse,
    ACTIVITY_CURSOR_INCARNATION_MISMATCH, ACTIVITY_CURSOR_TOO_OLD, ACTIVITY_SUBSCRIBER_LAGGED,
};

use super::{map, State};
use crate::model::{ActivityKey, ActivityLog, ActivityRow, Fault, ProcessEvent};
use crate::time::Timestamp;

/// How many entries one `activity.query` page asks for.
pub const PAGE: u32 = 100;

/// The most rows (entries and exits, a restart pair as one) kept loaded.
pub const CAP: usize = 2_000;

/// How many of the newest entries Home's activity list is built from.
const HOME_ENTRIES: usize = 200;

/// Where older entries come from next.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Older {
    /// No page has been read yet.
    #[default]
    Unread,
    /// The next older page starts below this seq (`activity.query`'s
    /// exclusive `cursor`).
    From(u64),
    /// The oldest loaded entry is the start of retained history.
    Exhausted,
}

/// The ledger as loaded, and the feed's position.
#[derive(Clone, Debug, Default)]
pub struct ActivityLedger {
    /// Loaded entries, newest first: by seq, then by daemon incarnation.
    pub entries: Vec<WireActivity>,
    /// Seqs of loaded entries with a "no longer kept" gap directly below
    /// them: the entries between them and the next loaded one are gone.
    pub gaps: BTreeSet<u64>,
    pub older: Older,
    /// Whether newer entries than the first loaded one exist but were
    /// dropped while the person read older pages. While they are, live
    /// entries aren't added (the loaded rows stay one range); the newest
    /// are read again when the view returns to the top.
    pub newest_dropped: bool,
    /// The last position the feed reached, where a new subscription
    /// resumes.
    pub head: Option<ActivityCursor>,
    /// Whether a subscription is open now.
    pub open: bool,
    /// The row the Activity view is on (its selection); `None` for the top.
    pub anchor: Option<ActivityKey>,
    /// The view reached the oldest loaded row and wants the next older
    /// page.
    pub want_older: bool,
}

/// A refill from the top in progress: the newest pages, read until they
/// reach an entry already loaded.
#[derive(Clone, Debug, Default)]
pub struct Refill {
    /// The newest seq loaded when the refill began; `None` when nothing was
    /// loaded, and the first page is all there is to read.
    stop_at: Option<u64>,
    /// The lowest seq this refill added.
    lowest: Option<u64>,
    /// Whether this refill replaces the feed's head with the first page's
    /// (the old one was refused, or there was none).
    take_head: bool,
    /// Whether a page has been applied.
    started: bool,
}

impl ActivityLedger {
    fn order(e: &WireActivity) -> (std::cmp::Reverse<u64>, &str) {
        (std::cmp::Reverse(e.seq), e.daemon_incarnation.as_str())
    }

    /// Adds `e` where it belongs in seq order. Returns false when it was
    /// already loaded.
    fn insert(&mut self, e: WireActivity) -> bool {
        match self
            .entries
            .binary_search_by(|x| Self::order(x).cmp(&Self::order(&e)))
        {
            Ok(_) => false,
            Err(at) => {
                self.entries.insert(at, e);
                true
            }
        }
    }

    fn top_seq(&self) -> Option<u64> {
        self.entries.first().map(|e| e.seq)
    }

    fn oldest_seq(&self) -> Option<u64> {
        self.entries.last().map(|e| e.seq)
    }

    /// Starts a refill from the top: the newest page, then older ones until
    /// they reach what is loaded.
    pub fn begin_refill(&self, take_head: bool) -> Refill {
        Refill {
            stop_at: self.top_seq(),
            lowest: None,
            take_head: take_head || self.head.is_none(),
            started: false,
        }
    }

    /// Merges one page of a refill. Returns the cursor of the next page to
    /// read, or `None` when the refill is done: it reached a loaded entry,
    /// or history ended first (and a gap label now separates the ranges).
    fn refill_page(&mut self, refill: &mut Refill, page: ActivityQueryResponse) -> Option<u64> {
        if refill.take_head && !refill.started {
            self.head = Some(page.head.clone());
        }
        refill.started = true;
        let reached = match refill.stop_at {
            None => true,
            Some(stop) => page.entries.iter().any(|e| e.seq <= stop),
        };
        for e in page.entries {
            // Below the loaded top the loaded range already says what is
            // kept; adding there could fill a gap the label marks.
            if refill.stop_at.is_some_and(|stop| e.seq <= stop) {
                continue;
            }
            refill.lowest = Some(refill.lowest.map_or(e.seq, |l| l.min(e.seq)));
            self.insert(e);
        }
        if refill.stop_at.is_none() {
            self.older = page.cursor.map_or(Older::Exhausted, Older::From);
        }
        let ended = page.cursor.is_none();
        if !reached && ended {
            if let Some(lowest) = refill.lowest {
                self.gaps.insert(lowest);
            }
        }
        if reached || ended {
            self.newest_dropped = false;
            None
        } else {
            page.cursor
        }
    }

    /// Merges an older page, read with the previous reply's cursor.
    fn older_page(&mut self, page: ActivityQueryResponse) {
        let floor = self.oldest_seq();
        for e in page.entries {
            if floor.is_none_or(|f| e.seq < f) {
                self.insert(e);
            }
        }
        self.older = page.cursor.map_or(Older::Exhausted, Older::From);
    }

    /// Takes one entry from the subscription. It moves the head; it is added
    /// unless the newest entries are dropped, when it will be read with them
    /// once the view returns to the top.
    fn live(&mut self, e: WireActivity) {
        self.head = Some(ActivityCursor {
            daemon_incarnation: e.daemon_incarnation.clone(),
            seq: e.seq,
        });
        if !self.newest_dropped {
            self.insert(e);
        }
    }

    /// The loaded rows, newest first: each contiguous run of entries (a
    /// restart and its start as one row), a label where entries are no
    /// longer kept, and the process exits that fall inside the loaded time.
    ///
    /// An exit older than the oldest loaded entry is held back until paging
    /// reaches its time or history is exhausted, so it never shows above
    /// entries not yet read. While the newest entries are dropped, exits
    /// newer than the newest loaded entry are held back the same way.
    pub fn rows(&self, exits: &[ProcessEvent], names: &HashMap<String, String>) -> ActivityLog {
        let mut ledger_rows = Vec::new();
        let mut start = 0;
        for (i, e) in self.entries.iter().enumerate() {
            if self.gaps.contains(&e.seq) && i + 1 < self.entries.len() {
                ledger_rows.extend(map::ledger_rows(&self.entries[start..=i], names));
                ledger_rows.push(map::gap_row(
                    e.seq,
                    Timestamp::from_unix_millis(e.at_ms as i64),
                ));
                start = i + 1;
            }
        }
        ledger_rows.extend(map::ledger_rows(&self.entries[start..], names));

        let exhausted = self.older == Older::Exhausted;
        let at = |e: &WireActivity| Timestamp::from_unix_millis(e.at_ms as i64);
        let floor = self.entries.last().map(at);
        let ceiling = self.entries.first().map(at).filter(|_| self.newest_dropped);
        let placed = exits.iter().filter(|x| {
            let above_floor = exhausted || floor.is_some_and(|f| x.at >= f);
            above_floor && ceiling.is_none_or(|c| x.at <= c)
        });
        // Both lists run newest first: each exit goes above the first entry
        // row older than it.
        let mut placed = placed.peekable();
        let mut rows: Vec<ActivityRow> = Vec::with_capacity(ledger_rows.len());
        for row in ledger_rows {
            while let Some(x) = placed.next_if(|x| x.at > row.at) {
                rows.push(map::exit_row(x));
            }
            rows.push(row);
        }
        rows.extend(placed.map(map::exit_row));
        ActivityLog {
            rows,
            history_exhausted: exhausted,
            newer_unloaded: self.newest_dropped,
        }
    }

    /// Drops rows past [`CAP`], keeping the ones nearest the anchor: the
    /// oldest go while the view is near the top, the newest while it is on
    /// older pages. Returns whether any of the newest went.
    fn trim(&mut self, exits: &[ProcessEvent]) -> bool {
        let log = self.rows(exits, &HashMap::new());
        let counted: Vec<&ActivityRow> = log.rows.iter().filter(|r| r.counts()).collect();
        let n = counted.len();
        if n <= CAP {
            return false;
        }
        let anchor = self
            .anchor
            .as_ref()
            .and_then(|k| counted.iter().position(|r| r.is(k)))
            .unwrap_or(0);
        let lo = anchor.saturating_sub(CAP / 2).min(n - CAP);
        let dropped = counted[..lo].iter().chain(&counted[lo + CAP..]);
        let gone: HashSet<&ActivityKey> = dropped
            .flat_map(|r| std::iter::once(&r.key).chain(r.partner.as_ref()))
            .collect();
        self.entries.retain(|e| !gone.contains(&map::entry_key(e)));
        let dropped_newest = lo > 0;
        if dropped_newest {
            self.newest_dropped = true;
        }
        if lo + CAP < n {
            if let Some(oldest) = self.oldest_seq() {
                self.older = Older::From(oldest);
            }
        }
        // A label marks the place between two loaded ranges; one with no
        // loaded entry on either side marks nothing.
        let oldest = self.oldest_seq();
        let loaded: HashSet<u64> = self.entries.iter().map(|e| e.seq).collect();
        self.gaps
            .retain(|s| loaded.contains(s) && oldest.is_some_and(|o| o < *s));
        dropped_newest
    }
}

impl State {
    /// Process exits as the Activity list shows them: the spawn feed's and
    /// the retained ones, the same exit from both listed once.
    pub fn activity_exits(&self) -> Vec<ProcessEvent> {
        let retained: Vec<ProcessEvent> = self.terminals.values().flatten().cloned().collect();
        map::process_exits(&self.process_events, &retained)
    }

    /// The Activity list's rows as now loaded.
    pub fn activity_log(&self, names: &HashMap<String, String>) -> ActivityLog {
        self.ledger.rows(&self.activity_exits(), names)
    }

    /// Home's activity list: the newest loaded entries.
    pub fn home_activity(
        &self,
        names: &HashMap<String, String>,
    ) -> Vec<crate::model::ActivityEntry> {
        let n = self.ledger.entries.len().min(HOME_ENTRIES);
        map::activity_rows(&self.ledger.entries[..n], names)
    }

    fn activity_read(&mut self, at: Timestamp) {
        self.activity.set((), at);
    }

    /// Records one page of a refill from the top, read at `at`, and returns
    /// the cursor of the next page to read, if the refill goes on. A refill
    /// stops early when keeping its rows would drop the person's row: the
    /// rest is read when the view returns to the top.
    pub fn record_refill_page(
        &mut self,
        refill: &mut Refill,
        page: ActivityQueryResponse,
        at: Timestamp,
    ) -> Option<u64> {
        let next = self.ledger.refill_page(refill, page);
        self.activity_read(at);
        let exits = self.activity_exits();
        if self.ledger.trim(&exits) {
            return None;
        }
        next
    }

    /// Records an older page, read at `at`.
    pub fn record_older_page(&mut self, page: ActivityQueryResponse, at: Timestamp) {
        self.ledger.older_page(page);
        self.activity_read(at);
        let exits = self.activity_exits();
        self.ledger.trim(&exits);
    }

    /// Records one entry from the subscription, received at `at`.
    pub fn record_live_activity(&mut self, e: WireActivity, at: Timestamp) {
        self.ledger.live(e);
        self.activity_read(at);
        let exits = self.activity_exits();
        self.ledger.trim(&exits);
    }

    /// Records that the subscription opened at `at`, which counts as hearing
    /// from the feed. An open feed never ages, however quiet.
    pub fn record_feed_open(&mut self, at: Timestamp) {
        self.ledger.open = true;
        self.activity.at = at;
        self.activity.error = None;
    }

    /// Where the Activity view is: the row it is on, and whether it reached
    /// the oldest loaded row and wants the next older page.
    pub fn set_activity_view(&mut self, anchor: Option<ActivityKey>, older: bool) {
        self.ledger.anchor = anchor;
        if older {
            self.ledger.want_older = true;
        }
    }

    /// The cursor of the older page the view asked for, if one is due: the
    /// view asked, a page has been read, and history goes on. Asking is
    /// answered once.
    pub fn take_older_wanted(&mut self) -> Option<u64> {
        if !std::mem::take(&mut self.ledger.want_older) {
            return None;
        }
        match self.ledger.older {
            Older::From(c) => Some(c),
            _ => None,
        }
    }

    /// Whether the newest entries were dropped and the view is back at the
    /// top, so they are due to be read again.
    pub fn top_refill_wanted(&self) -> bool {
        if !self.ledger.newest_dropped {
            return false;
        }
        let Some(anchor) = &self.ledger.anchor else {
            return true;
        };
        let log = self.activity_log(&HashMap::new());
        log.rows
            .iter()
            .find(|r| r.counts())
            .is_some_and(|r| r.is(anchor))
    }
}

// ---- the ledger's source ---------------------------------------------------------

/// Why a feed ended, or couldn't open.
#[derive(Debug)]
pub enum FeedEnd {
    /// The stream ended without an error, as it does when the connection
    /// goes away.
    Ended,
    /// The daemon dropped a subscriber that fell behind. Subscribing again
    /// from the last head replays what was missed.
    Lagged,
    /// The daemon refused the cursor: it belongs to another daemon
    /// incarnation, or entries after it have left the daemon's replay
    /// ring. The newest pages are read instead, down to what is loaded.
    CursorRefused,
    Failed(Fault),
}

impl FeedEnd {
    /// Sorts a failure by the daemon's refusal code; any other failure is
    /// `fault`.
    pub fn from_code(code: Option<&str>, fault: Fault) -> FeedEnd {
        match code {
            Some(ACTIVITY_CURSOR_TOO_OLD | ACTIVITY_CURSOR_INCARNATION_MISMATCH) => {
                FeedEnd::CursorRefused
            }
            Some(ACTIVITY_SUBSCRIBER_LAGGED) => FeedEnd::Lagged,
            _ => FeedEnd::Failed(fault),
        }
    }
}

/// Where ledger pages and the live feed come from: the daemon, or a
/// scripted ledger in tests.
pub trait LedgerSource: Send + Sync {
    /// One `activity.query` page of up to [`PAGE`] entries, newest first,
    /// below `cursor` when given.
    fn query(&self, cursor: Option<u64>) -> BoxFuture<'_, Result<ActivityQueryResponse, Fault>>;
    /// `activity.subscribe` from just after `since`.
    fn subscribe(
        &self,
        since: Option<ActivityCursor>,
    ) -> BoxFuture<'_, Result<Box<dyn LedgerFeed>, FeedEnd>>;
}

/// An open activity subscription.
pub trait LedgerFeed: Send {
    /// The next entry, or why the feed ended.
    fn next(&mut self) -> BoxFuture<'_, Result<WireActivity, FeedEnd>>;
}
