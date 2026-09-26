//! One log line when a module starts being refused for not serving, and one
//! when it is accepted again.
//!
//! Every `route.open` refusal already logs an INFO line, so a module that is
//! down without crashing (a slow restart, a spawned process that has not
//! registered yet, a registration that lapsed) produced no WARN or ERROR at
//! all: a scan for warnings over a log with dozens of refusals in a row found
//! nothing. A crash is already reported at WARN by the supervisor; this covers
//! the outages with no crash behind them.
//!
//! The shape follows the bind-relay breaker: the per-refusal record stays
//! where it was, and only the two edges of an outage get a line here. The
//! level of the opening line says whether anybody asked for the outage:
//!
//! - INFO when an operator action on that module (stop, restart, reload, swap,
//!   enable or disable, including one applied by a rescan) is what began it,
//!   or when the module has not been accepted once since this daemon started,
//!   which is the normal state of every module while the daemon boots;
//! - WARN otherwise. That deliberately includes health-probe restarts, crash
//!   respawns and lapsed registrations: the daemon took no instruction to make
//!   the module unavailable, and a health restart means the module was faulty,
//!   which is exactly what a warning scan exists to find.
//!
//! Memory stays bounded because callers only report modules the daemon
//! already knows (configured, or registered at least once): a client asking
//! for an arbitrary id is refused before it reaches this tracker. Outage
//! entries and operator marks are removed on recovery, and a module removed
//! from the configuration is forgotten entirely.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::Instant,
};

use tracing::{info, warn};

/// Who began the not-serving period that opened an outage. Rendered as the
/// `initiated_by` field on both edge lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutageInitiator {
    /// An operator action on this module was in progress or had completed
    /// without the module being accepted since.
    Operator,
    /// The module has never been accepted since this daemon started.
    DaemonStartup,
    /// Nothing the daemon was asked to do explains the outage.
    Unexplained,
}

impl OutageInitiator {
    fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::DaemonStartup => "daemon_startup",
            Self::Unexplained => "unexplained",
        }
    }
}

#[derive(Debug)]
struct Outage {
    started: Instant,
    /// The refusal label of the first refusal; later refusals in the same
    /// outage may carry a different one (draining, then not registered) and
    /// are not recorded, because the first says how it began.
    reason: &'static str,
    initiator: OutageInitiator,
    refused: u64,
}

#[derive(Debug, Default)]
struct State {
    outages: HashMap<String, Outage>,
    /// Modules an operator action is currently making, or has just made,
    /// unavailable. Read only when an outage opens; cleared when the module is
    /// accepted, or when the action fails before any refusal was seen.
    operator_marks: HashSet<String>,
    /// Modules accepted at least once since the daemon started. A module not
    /// in here is still coming up for the first time.
    accepted_once: HashSet<String>,
}

/// Per-module outage edges for `route.open`. One lock guards every decision
/// so that however many opens race, exactly one of them opens an outage and
/// exactly one closes it.
#[derive(Debug, Default)]
pub(crate) struct RouteOutageTracker {
    state: Mutex<State>,
}

impl RouteOutageTracker {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Count a refusal of a known module because it is not serving, logging
    /// the opening line if this refusal is the first of an outage.
    ///
    /// The line is emitted under the lock: the decision and the line must be
    /// one step, or a racing accept could log the recovery before the start.
    /// This only happens once per outage, so holding the lock across it costs
    /// nothing on the per-refusal path.
    pub(crate) fn record_not_serving(&self, module_id: &str, reason: &'static str) {
        let mut state = self.lock();
        if let Some(outage) = state.outages.get_mut(module_id) {
            outage.refused = outage.refused.saturating_add(1);
            return;
        }
        let initiator = if state.operator_marks.contains(module_id) {
            OutageInitiator::Operator
        } else if !state.accepted_once.contains(module_id) {
            OutageInitiator::DaemonStartup
        } else {
            OutageInitiator::Unexplained
        };
        state.outages.insert(
            module_id.to_string(),
            Outage {
                started: Instant::now(),
                reason,
                initiator,
                refused: 1,
            },
        );
        // `module_id` is Debug-formatted like every refusal line: it names a
        // known module, but the escaping keeps the two lines greppable alike.
        if initiator == OutageInitiator::Unexplained {
            warn!(
                target: "control",
                module_id = ?module_id,
                reason,
                initiated_by = initiator.as_str(),
                "route.open refusing module: not serving"
            );
        } else {
            info!(
                target: "control",
                module_id = ?module_id,
                reason,
                initiated_by = initiator.as_str(),
                "route.open refusing module: not serving"
            );
        }
    }

    /// Record an accepted `route.open`, closing the module's outage with one
    /// line if it had one. Any operator mark is spent: the action it
    /// explained is over once the module serves again.
    pub(crate) fn record_accepted(&self, module_id: &str) {
        let mut state = self.lock();
        state.operator_marks.remove(module_id);
        if !state.accepted_once.contains(module_id) {
            state.accepted_once.insert(module_id.to_string());
        }
        let Some(outage) = state.outages.remove(module_id) else {
            return;
        };
        info!(
            target: "control",
            module_id = ?module_id,
            reason = outage.reason,
            initiated_by = outage.initiator.as_str(),
            duration_ms = u64::try_from(outage.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            refused = outage.refused,
            "route.open accepted again after module outage"
        );
    }

    /// Note that an operator action is about to make `module_id` unavailable.
    /// Call it before acting, so refusals seen while the action runs are
    /// already attributed to it, and only for a module the supervisor knows.
    pub(crate) fn mark_operator_action(&self, module_id: &str) {
        let mut state = self.lock();
        if !state.operator_marks.contains(module_id) {
            state.operator_marks.insert(module_id.to_string());
        }
    }

    /// The operator action on `module_id` failed, or turned out to change
    /// nothing. If no refusal was seen while it ran, the module never looked
    /// unavailable, and a mark left behind would make a later, unrelated
    /// outage read as requested. If an outage is already open the mark has
    /// done its job and is spent on recovery like any other.
    pub(crate) fn operator_action_ended_unrefused(&self, module_id: &str) {
        let mut state = self.lock();
        if !state.outages.contains_key(module_id) {
            state.operator_marks.remove(module_id);
        }
    }

    /// Drop everything known about a module that was removed from the
    /// configuration. An outage open at that point ends without a recovery
    /// line: the module did not come back, it was retired on purpose.
    pub(crate) fn forget(&self, module_id: &str) {
        let mut state = self.lock();
        state.outages.remove(module_id);
        state.operator_marks.remove(module_id);
        state.accepted_once.remove(module_id);
    }

    #[cfg(test)]
    pub(crate) fn tracked_module_count(&self) -> usize {
        let state = self.lock();
        state
            .outages
            .keys()
            .chain(state.operator_marks.iter())
            .chain(state.accepted_once.iter())
            .collect::<HashSet<_>>()
            .len()
    }

    #[cfg(test)]
    pub(crate) fn has_operator_mark(&self, module_id: &str) -> bool {
        self.lock().operator_marks.contains(module_id)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fmt,
        sync::{Arc, Barrier, Mutex},
        thread,
        time::Duration,
    };

    use tracing::{
        field::{Field, Visit},
        Event, Level, Subscriber,
    };
    use tracing_subscriber::{
        layer::{Context, Layer},
        prelude::*,
    };

    use super::RouteOutageTracker;

    #[derive(Clone, Debug)]
    struct Captured {
        level: Level,
        message: String,
        fields: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct Capture {
        events: Arc<Mutex<Vec<Captured>>>,
    }

    impl Capture {
        fn lines(&self, message: &str) -> Vec<Captured> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.message == message)
                .cloned()
                .collect()
        }
    }

    #[derive(Default)]
    struct Fields(BTreeMap<String, String>);

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            let message = fields.0.remove("message").unwrap_or_default();
            self.events.lock().unwrap().push(Captured {
                level: *event.metadata().level(),
                message,
                fields: fields.0,
            });
        }
    }

    const START: &str = "route.open refusing module: not serving";
    const RECOVERED: &str = "route.open accepted again after module outage";

    fn dispatch(capture: &Capture) -> tracing::Dispatch {
        tracing::Dispatch::new(tracing_subscriber::registry().with(capture.clone()))
    }

    /// Many threads refusing the same module at once must produce one opening
    /// line between them, and the accept that follows one closing line that
    /// counts every refusal.
    #[test]
    fn concurrent_refusals_open_one_outage_and_one_accept_closes_it() {
        const REFUSALS: usize = 32;
        let capture = Capture::default();
        let dispatch = dispatch(&capture);
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let tracker = Arc::new(RouteOutageTracker::default());
        tracker.record_accepted("m");

        let barrier = Arc::new(Barrier::new(REFUSALS));
        thread::scope(|scope| {
            for _ in 0..REFUSALS {
                let tracker = Arc::clone(&tracker);
                let barrier = Arc::clone(&barrier);
                let dispatch = dispatch.clone();
                scope.spawn(move || {
                    let _guard = tracing::dispatcher::set_default(&dispatch);
                    barrier.wait();
                    tracker.record_not_serving("m", "supervisor_not_live");
                });
            }
        });
        thread::sleep(Duration::from_millis(5));
        tracker.record_accepted("m");

        let starts = capture.lines(START);
        assert_eq!(starts.len(), 1, "one opening line: {starts:?}");
        assert_eq!(starts[0].fields["reason"], "\"supervisor_not_live\"");
        assert_eq!(starts[0].fields["module_id"], "\"m\"");
        let recoveries = capture.lines(RECOVERED);
        assert_eq!(recoveries.len(), 1, "one closing line: {recoveries:?}");
        let recovery = &recoveries[0];
        assert_eq!(recovery.level, Level::INFO);
        assert_eq!(recovery.fields["refused"], REFUSALS.to_string());
        assert_eq!(recovery.fields["reason"], "\"supervisor_not_live\"");
        let duration_ms: u64 = recovery.fields["duration_ms"].parse().unwrap();
        assert!(
            (5..10_000).contains(&duration_ms),
            "duration_ms {duration_ms} should cover the pause before the accept"
        );

        // A second accept with no outage open logs nothing more, and the
        // closed outage left no entry behind beyond the accepted-once record.
        tracker.record_accepted("m");
        assert_eq!(capture.lines(RECOVERED).len(), 1);
        assert_eq!(tracker.tracked_module_count(), 1);
    }

    /// The opening line's level separates requested outages from the ones a
    /// warning scan must find.
    #[test]
    fn outage_level_depends_on_who_began_it() {
        let capture = Capture::default();
        let _guard = tracing::dispatcher::set_default(&dispatch(&capture));
        let tracker = RouteOutageTracker::default();

        // Never accepted yet: still coming up with the daemon.
        tracker.record_not_serving("booting", "supervised_not_registered");
        // Served before, then an operator restart.
        tracker.record_accepted("restarted");
        tracker.mark_operator_action("restarted");
        tracker.record_not_serving("restarted", "reloading");
        // Served before, and nobody asked for it to stop.
        tracker.record_accepted("lapsed");
        tracker.record_not_serving("lapsed", "no_forwarding_connection");

        let starts = capture.lines(START);
        let by_module = |id: &str| {
            starts
                .iter()
                .find(|event| event.fields["module_id"] == format!("{id:?}"))
                .unwrap_or_else(|| panic!("no opening line for {id}: {starts:?}"))
                .clone()
        };
        let booting = by_module("booting");
        assert_eq!(booting.level, Level::INFO);
        assert_eq!(booting.fields["initiated_by"], "\"daemon_startup\"");
        let restarted = by_module("restarted");
        assert_eq!(restarted.level, Level::INFO);
        assert_eq!(restarted.fields["initiated_by"], "\"operator\"");
        let lapsed = by_module("lapsed");
        assert_eq!(lapsed.level, Level::WARN);
        assert_eq!(lapsed.fields["initiated_by"], "\"unexplained\"");
    }

    /// An operator action that failed before the module was ever refused must
    /// not leave a mark that would later excuse a real outage.
    #[test]
    fn failed_operator_action_leaves_no_mark_behind() {
        let capture = Capture::default();
        let _guard = tracing::dispatcher::set_default(&dispatch(&capture));
        let tracker = RouteOutageTracker::default();
        tracker.record_accepted("m");

        tracker.mark_operator_action("m");
        tracker.operator_action_ended_unrefused("m");
        assert!(!tracker.has_operator_mark("m"));
        tracker.record_not_serving("m", "supervisor_not_live");
        let starts = capture.lines(START);
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].level, Level::WARN);

        // With the outage already open, a failure keeps the mark: it is spent
        // on recovery instead.
        tracker.mark_operator_action("m");
        tracker.operator_action_ended_unrefused("m");
        assert!(tracker.has_operator_mark("m"));
        tracker.record_accepted("m");
        assert!(!tracker.has_operator_mark("m"));
    }

    #[test]
    fn forget_drops_every_record_of_a_removed_module() {
        let tracker = RouteOutageTracker::default();
        tracker.record_accepted("gone");
        tracker.mark_operator_action("gone");
        tracker.record_not_serving("gone", "supervisor_not_live");
        tracker.forget("gone");
        assert_eq!(tracker.tracked_module_count(), 0);
    }
}
