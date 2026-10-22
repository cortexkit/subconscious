//! Which sources run, decided by what is on screen.
//!
//! The daemon's own sources (the roster, health, the spawn feed,
//! provenance, `server.describe`, routes, the log on screen) and the host's
//! facts run on every screen, because following an action and the Modules
//! screen depend on them. Each module source, and the activity ledger with
//! the daemon's retained exits, runs only while a screen that shows its data
//! is on screen: [`wanted`] is that rule.
//!
//! The app tells the live layer what it shows ([`Showing`]); the live layer
//! publishes the matching [`Sources`] on a watch channel, and every source
//! task asks its [`Gate`] before it reads. A polled source that stops being
//! wanted starts no new call (one already in flight still lands) and keeps
//! its last reading and its age; when it is wanted again it reads at once
//! rather than waiting out its interval ([`gated_poll`]). A subscription is
//! closed while unwanted and opened again when wanted ([`gated_session`]);
//! the session itself decides where to resume from.

use std::future::Future;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{every, modules};
use crate::model::Tab;

/// One thing the live layer reads or follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    // Run on every screen.
    /// `supervisor.list`.
    Roster,
    /// `supervisor.health`, and the direct probes of the selected module and
    /// of modules whose cached metrics are only the cache's size marker.
    Health,
    /// The spawn snapshot, then the spawn subscription.
    SpawnFeed,
    /// `supervisor.provenance`.
    Provenance,
    /// `server.describe`.
    Describe,
    /// `supervisor.routes`, for the Modules screen's routes column.
    Routes,
    /// Memory, load and disk, read on this machine.
    Host,
    /// The selected module's log (its files, or the stderr tail the daemon
    /// keeps).
    Log,

    // Run only while a screen that shows them is on screen.
    /// insula `usage.get`.
    Usage,
    /// callosum `callosum.devices_read`.
    Devices,
    /// entorhinal `agent.fleet_identity`.
    Identity,
    /// prefrontal-core `agent.fleet_residence`.
    Residence,
    /// entorhinal `agent.where_many`, for where each agent lives.
    Places,
    /// entorhinal `enumerate`, for the fleet's project and workspace counts.
    Counts,
    /// The activity ledger: its newest page, then its subscription.
    Activity,
    /// `supervisor.terminals`: each module's retained process exits.
    Terminals,
    /// callosum `callosum.pairing_qr_watch` for the window the Add a phone
    /// overlay shows. A watch held after the overlay closes, because its
    /// confirm is still in flight, belongs to the pairing exchange and is not
    /// governed by what shows.
    PairingWatch,
    /// The `plan_upgrade` check. It runs once per session, the first time
    /// Settings → Updates shows ([`gated_once`]).
    UpgradeCheck,
}

impl Source {
    pub const ALL: [Source; 18] = [
        Source::Roster,
        Source::Health,
        Source::SpawnFeed,
        Source::Provenance,
        Source::Describe,
        Source::Routes,
        Source::Host,
        Source::Log,
        Source::Usage,
        Source::Devices,
        Source::Identity,
        Source::Residence,
        Source::Places,
        Source::Counts,
        Source::Activity,
        Source::Terminals,
        Source::PairingWatch,
        Source::UpgradeCheck,
    ];

    const fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// A set of sources.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Sources(u32);

impl Sources {
    pub const EMPTY: Sources = Sources(0);

    pub const fn of(sources: &[Source]) -> Sources {
        let mut bits = 0;
        let mut i = 0;
        while i < sources.len() {
            bits |= sources[i].bit();
            i += 1;
        }
        Sources(bits)
    }

    pub const fn contains(self, source: Source) -> bool {
        self.0 & source.bit() != 0
    }

    pub const fn with(self, source: Source) -> Sources {
        Sources(self.0 | source.bit())
    }

    pub const fn union(self, other: Sources) -> Sources {
        Sources(self.0 | other.0)
    }

    pub fn iter(self) -> impl Iterator<Item = Source> {
        Source::ALL.into_iter().filter(move |s| self.contains(*s))
    }
}

/// The sources that run on every screen.
pub const ALWAYS: Sources = Sources::of(&[
    Source::Roster,
    Source::Health,
    Source::SpawnFeed,
    Source::Provenance,
    Source::Describe,
    Source::Routes,
    Source::Host,
    Source::Log,
]);

/// What the app has on screen, as far as the sources are concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Showing {
    /// The tab on screen.
    pub tab: Tab,
    /// The Add a phone overlay is open. It reads `callosum.devices_read`
    /// for this Mac's access groups.
    pub add_phone: bool,
    /// The Add a phone overlay shows a pairing window (its code, or a later
    /// step for the same window), whose watch must stay open.
    pub pairing_window: bool,
    /// Settings → Updates is the section on screen.
    pub updates: bool,
}

impl Showing {
    /// A tab with nothing over it.
    pub const fn tab(tab: Tab) -> Showing {
        Showing {
            tab,
            add_phone: false,
            pairing_window: false,
            updates: false,
        }
    }
}

impl Default for Showing {
    /// The app opens on Home.
    fn default() -> Showing {
        Showing::tab(Tab::Home)
    }
}

/// The sources to run while `showing` is on screen: [`ALWAYS`], plus each
/// module source a screen on screen shows.
pub fn wanted(showing: &Showing) -> Sources {
    use Source::*;
    let by_tab: &[Source] = match showing.tab {
        // Home shows a little of everything.
        Tab::Home => &[
            Usage, Devices, Identity, Residence, Places, Counts, Activity, Terminals,
        ],
        // The starts and exits list.
        Tab::Modules => &[Activity, Terminals],
        // Identity names each entry's actor.
        Tab::Activity => &[Activity, Terminals, Identity],
        Tab::Usage => &[Usage],
        Tab::Agents => &[Identity, Residence, Places, Counts],
        Tab::Devices => &[Devices],
        Tab::Settings => &[],
    };
    let mut out = ALWAYS.union(Sources::of(by_tab));
    if showing.add_phone {
        out = out.with(Devices);
    }
    if showing.add_phone && showing.pairing_window {
        out = out.with(PairingWatch);
    }
    if showing.tab == Tab::Settings && showing.updates {
        out = out.with(UpgradeCheck);
    }
    out
}

// ---- the gate --------------------------------------------------------------------

/// A source task's view of which sources are wanted.
#[derive(Clone, Debug)]
pub struct Gate {
    rx: watch::Receiver<Sources>,
}

impl Gate {
    pub fn new(rx: watch::Receiver<Sources>) -> Gate {
        Gate { rx }
    }

    pub fn wanted(&self, source: Source) -> bool {
        self.rx.borrow().contains(source)
    }

    /// Returns once `source` is wanted, at once if it is now.
    pub async fn until_wanted(&mut self, source: Source) {
        self.until(|s| s.contains(source)).await;
    }

    /// Returns once `source` is no longer wanted, at once if it isn't now.
    pub async fn until_unwanted(&mut self, source: Source) {
        self.until(|s| !s.contains(source)).await;
    }

    /// Returns when `source`, wanted or not now, stops being wanted and is
    /// then wanted again: the moment a paused source's screen shows again.
    pub async fn until_resumed(&mut self, source: Source) {
        self.until_unwanted(source).await;
        self.until_wanted(source).await;
    }

    async fn until(&mut self, f: impl FnMut(&Sources) -> bool) {
        // The sender lives as long as the live layer; once it is gone
        // nothing changes again, so wait forever rather than spin.
        if self.rx.wait_for(f).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Reads `source` with `read` while it is wanted, waiting `period` after each
/// reading completes. A reading in flight when `source` stops being wanted is
/// not cancelled; after it, nothing is read until `source` is wanted again,
/// and then it reads at once, so a screen that shows again gets a fresh
/// reading without waiting out the interval and keeps showing the last one
/// until it lands. A pause and resume that both happen while a reading is in
/// flight go unnoticed, since that reading is itself fresh.
pub fn gated_poll<F, Fut>(
    gate: &Gate,
    source: Source,
    period: Duration,
    mut read: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let mut gate = gate.clone();
    tokio::spawn(async move {
        loop {
            gate.until_wanted(source).await;
            read().await;
            tokio::select! {
                _ = tokio::time::sleep(period) => {}
                _ = gate.until_resumed(source) => {}
            }
        }
    })
}

/// Runs `session` while `source` is wanted. A session is one subscription
/// from opening to its end; when it ends it is started again. When the
/// source stops being wanted the session is dropped, which closes its
/// subscription, and a new one starts when it is wanted again. Where to
/// resume from is the session's to remember.
pub fn gated_session<F, Fut>(gate: &Gate, source: Source, mut session: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let mut gate = gate.clone();
    tokio::spawn(async move {
        loop {
            gate.until_wanted(source).await;
            let mut paused = gate.clone();
            tokio::select! {
                _ = session() => {}
                _ = paused.until_unwanted(source) => {}
            }
        }
    })
}

/// Runs `read` once, the first time `source` is wanted.
pub fn gated_once<Fut>(gate: &Gate, source: Source, read: Fut) -> JoinHandle<()>
where
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut gate = gate.clone();
    tokio::spawn(async move {
        gate.until_wanted(source).await;
        read.await;
    })
}

// ---- module sources --------------------------------------------------------------

/// A module source read with one `{"method", "params"}` request on a
/// schedule.
#[derive(Clone, Copy, Debug)]
pub struct Polled {
    pub source: Source,
    /// The module's id, as the daemon knows it.
    pub module: &'static str,
    pub method: &'static str,
    /// Seconds to wait after one reading completes before the next.
    pub every: f64,
}

/// The module sources read with a single request each. `agent.where_many`
/// is not here: it waits for identity rows and sends one request per batch
/// of ids.
pub const POLLED: [Polled; 5] = [
    Polled {
        source: Source::Usage,
        module: modules::USAGE,
        method: "usage.get",
        every: every::USAGE,
    },
    Polled {
        source: Source::Devices,
        module: modules::DEVICES,
        method: "callosum.devices_read",
        every: every::DEVICES,
    },
    Polled {
        source: Source::Identity,
        module: modules::IDENTITY,
        method: "agent.fleet_identity",
        every: every::AGENTS,
    },
    Polled {
        source: Source::Residence,
        module: modules::RESIDENCE,
        method: "agent.fleet_residence",
        every: every::AGENTS,
    },
    Polled {
        source: Source::Counts,
        module: modules::IDENTITY,
        method: "enumerate",
        every: every::COUNTS,
    },
];

/// Starts one [`gated_poll`] task per [`POLLED`] source. Each reading sends
/// `call(module, method, params)` and hands the reply to `apply`.
pub fn module_sources<C, CFut, A>(gate: &Gate, call: C, apply: A) -> Vec<JoinHandle<()>>
where
    C: Fn(&'static str, &'static str, Value) -> CFut + Clone + Send + 'static,
    CFut: Future<Output = Result<Value, crate::model::Fault>> + Send,
    A: Fn(Source, Result<Value, crate::model::Fault>) + Clone + Send + 'static,
{
    POLLED
        .iter()
        .map(|p| {
            let p = *p;
            let call = call.clone();
            let apply = apply.clone();
            gated_poll(
                gate,
                p.source,
                Duration::from_secs_f64(p.every),
                move || {
                    let reply = call(p.module, p.method, json!({}));
                    let apply = apply.clone();
                    async move {
                        let reply = reply.await;
                        apply(p.source, reply)
                    }
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::live::{assemble, State};
    use crate::model::{Fault, Freshness};
    use crate::time::Timestamp;

    /// The module requests a scripted run sent: method and seconds since
    /// the run began.
    type Sent = Arc<Mutex<Vec<(&'static str, f64)>>>;

    /// A scripted live layer on tokio's paused clock: the module sources
    /// [`module_sources`] starts, answering `callosum.devices_read` with a
    /// device list after `reply_after` and refusing everything else, with
    /// every reply recorded into one [`State`] at the scripted time.
    struct Scripted {
        demand: watch::Sender<Sources>,
        gate: Gate,
        sent: Sent,
        state: Arc<Mutex<State>>,
        began: tokio::time::Instant,
        tasks: Vec<JoinHandle<()>>,
    }

    const BASE_MS: i64 = 1_700_000_000_000;

    fn devices_reply() -> Value {
        json!({
            "devices": [
                { "name": "Mac 2", "fingerprint": "f1", "device_class": "mac", "trust": "verified",
                  "tunnel": "established", "carrier": "direct", "since_unix_ms": BASE_MS,
                  "last_seen_unix_ms": BASE_MS, "subscriptions_live": 0 }
            ],
            "self": { "listen_addr": "0.0.0.0:1", "lan_addrs": [], "rendezvous": null,
                      "wan_reachability": "unknown", "wan_port_mapping": null,
                      "watch_ops_offered": [] }
        })
    }

    impl Scripted {
        fn start(showing: Showing, reply_after: Duration) -> Scripted {
            let demand = watch::Sender::new(wanted(&showing));
            let gate = Gate::new(demand.subscribe());
            let sent: Sent = Arc::default();
            let state = Arc::new(Mutex::new(State::empty("This Mac")));
            let began = tokio::time::Instant::now();
            let call = {
                let sent = Arc::clone(&sent);
                move |_module: &'static str, method: &'static str, _params: Value| {
                    sent.lock()
                        .unwrap()
                        .push((method, began.elapsed().as_secs_f64()));
                    async move {
                        tokio::time::sleep(reply_after).await;
                        if method == "callosum.devices_read" {
                            Ok(devices_reply())
                        } else {
                            Err(Fault::code("not_scripted"))
                        }
                    }
                }
            };
            let apply = {
                let state = Arc::clone(&state);
                move |source: Source, reply: Result<Value, Fault>| {
                    let at = clock(began);
                    state.lock().unwrap().record_module_reply(source, reply, at);
                }
            };
            let tasks = module_sources(&gate, call, apply);
            Scripted {
                demand,
                gate,
                sent,
                state,
                began,
                tasks,
            }
        }

        fn show(&self, showing: Showing) {
            self.demand.send_replace(wanted(&showing));
        }

        /// When each `callosum.devices_read` was sent, in seconds.
        fn devices_reads(&self) -> Vec<f64> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _)| *m == "callosum.devices_read")
                .map(|(_, t)| *t)
                .collect()
        }

        fn devices_panel(&self) -> crate::model::Panel<crate::model::Devices> {
            assemble(&self.state.lock().unwrap(), clock(self.began)).devices
        }

        fn now(&self) -> Timestamp {
            clock(self.began)
        }

        /// Starts a session task for `source` that counts the seconds it
        /// runs, and returns how many sessions began and how many seconds
        /// they ran.
        fn session(&mut self, source: Source) -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let began = Arc::new(AtomicUsize::new(0));
            let ran = Arc::new(AtomicUsize::new(0));
            let (b, r) = (Arc::clone(&began), Arc::clone(&ran));
            self.tasks.push(gated_session(&self.gate, source, move || {
                b.fetch_add(1, Ordering::SeqCst);
                let r = Arc::clone(&r);
                async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        r.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
            (began, ran)
        }
    }

    impl Drop for Scripted {
        fn drop(&mut self) {
            for t in &self.tasks {
                t.abort();
            }
        }
    }

    /// The scripted wall clock: `BASE_MS` when the run began, advancing
    /// with tokio's paused clock.
    fn clock(began: tokio::time::Instant) -> Timestamp {
        Timestamp::from_unix_millis(BASE_MS + began.elapsed().as_millis() as i64)
    }

    async fn run_for(secs: f64) {
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn started_on_home_devices_are_read_within_15_s_and_the_row_is_not_dimmed_after_75_s() {
        let run = Scripted::start(Showing::tab(Tab::Home), Duration::ZERO);
        run_for(15.0).await;
        let first = run.devices_reads();
        assert!(
            first.first().is_some_and(|t| *t <= 15.0),
            "a devices_read within 15 s of starting on Home: {first:?}"
        );
        run_for(75.0).await;
        let p = run.devices_panel();
        assert!(p.loaded, "the devices panel has a reading");
        assert_ne!(
            p.freshness(run.now()),
            Freshness::Stale,
            "the devices row is not dimmed 90 s in; reads at {:?}",
            run.devices_reads()
        );
        assert_eq!(p.freshness(run.now()), Freshness::Fresh);
    }

    #[tokio::test(start_paused = true)]
    async fn moving_to_usage_stops_the_devices_source_and_keeps_the_spawn_feed_running() {
        let mut run = Scripted::start(Showing::tab(Tab::Home), Duration::ZERO);
        let (spawn_sessions, spawn_ran) = run.session(Source::SpawnFeed);
        let (activity_sessions, activity_ran) = run.session(Source::Activity);
        run_for(20.0).await;
        assert!(!run.devices_reads().is_empty());

        run.show(Showing::tab(Tab::Usage));
        run_for(0.5).await;
        let devices_before = run.devices_reads().len();
        let spawn_before = spawn_ran.load(Ordering::SeqCst);
        let activity_before = activity_ran.load(Ordering::SeqCst);
        run_for(120.0).await;

        assert_eq!(
            run.devices_reads().len(),
            devices_before,
            "no devices_read while Usage shows: {:?}",
            run.devices_reads()
        );
        assert!(
            spawn_ran.load(Ordering::SeqCst) >= spawn_before + 119,
            "the spawn feed keeps running on Usage"
        );
        assert_eq!(
            spawn_sessions.load(Ordering::SeqCst),
            1,
            "the spawn feed was never closed and reopened"
        );
        assert_eq!(
            activity_ran.load(Ordering::SeqCst),
            activity_before,
            "the activity feed is closed while Usage shows"
        );
        // Back on Home, the activity feed opens a new session.
        run.show(Showing::tab(Tab::Home));
        run_for(2.5).await;
        assert_eq!(activity_sessions.load(Ordering::SeqCst), 2);
        assert!(activity_ran.load(Ordering::SeqCst) > activity_before);
    }

    #[tokio::test(start_paused = true)]
    async fn a_paused_source_reads_at_once_when_its_screen_shows_again_and_keeps_its_last_reading_until_then(
    ) {
        // Each devices_read takes a second to answer: sent at 0 s and 16 s,
        // the second landing at 17 s, the next due at 32 s.
        let run = Scripted::start(Showing::tab(Tab::Home), Duration::from_secs(1));
        run_for(20.0).await;
        assert!(!run.devices_reads().is_empty());

        // Home comes back before the next reading was due.
        run.show(Showing::tab(Tab::Modules));
        run_for(5.0).await;
        let before = run.devices_panel();
        let reads = run.devices_reads().len();
        run.show(Showing::tab(Tab::Home));
        run_for(0.5).await;
        let after = run.devices_reads();
        assert_eq!(after.len(), reads + 1, "read at once: {after:?}");
        assert_eq!(after.last(), Some(&25.0), "read at once: {after:?}");
        let p = run.devices_panel();
        assert!(p.loaded, "the last reading stays until the new one lands");
        assert_eq!(p.received_at, before.received_at, "and so does its age");
        assert_eq!(p.data, before.data);

        run_for(1.0).await;
        assert_eq!(
            run.devices_panel().received_at,
            Timestamp::from_unix_millis(BASE_MS + 26_000)
        );
    }

    #[test]
    fn each_module_source_runs_only_on_the_screens_that_show_it() {
        use Source::*;
        let on = |showing: Showing| -> Vec<Source> {
            let w = wanted(&showing);
            assert!(
                ALWAYS.iter().all(|s| w.contains(s)),
                "the daemon's sources run on every screen: {showing:?}"
            );
            w.iter().filter(|s| !ALWAYS.contains(*s)).collect()
        };
        let tab = Showing::tab;
        assert_eq!(
            on(tab(Tab::Home)),
            [Usage, Devices, Identity, Residence, Places, Counts, Activity, Terminals]
        );
        assert_eq!(on(tab(Tab::Modules)), [Activity, Terminals]);
        assert_eq!(on(tab(Tab::Activity)), [Identity, Activity, Terminals]);
        assert_eq!(on(tab(Tab::Usage)), [Usage]);
        assert_eq!(on(tab(Tab::Agents)), [Identity, Residence, Places, Counts]);
        assert_eq!(on(tab(Tab::Devices)), [Devices]);
        assert_eq!(on(tab(Tab::Settings)), []);

        // Add a phone reads the access groups, and watches the window it
        // shows.
        let overlay = Showing {
            add_phone: true,
            ..tab(Tab::Devices)
        };
        assert_eq!(on(overlay), [Devices]);
        let window = Showing {
            pairing_window: true,
            ..overlay
        };
        assert_eq!(on(window), [Devices, PairingWatch]);
        // A window with the overlay closed watches nothing on its behalf.
        let closed = Showing {
            add_phone: false,
            ..window
        };
        assert_eq!(on(closed), [Devices]);

        let updates = Showing {
            updates: true,
            ..tab(Tab::Settings)
        };
        assert_eq!(on(updates), [UpgradeCheck]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_upgrade_check_runs_once_when_settings_updates_first_shows() {
        let updates = Showing {
            updates: true,
            ..Showing::tab(Tab::Settings)
        };
        let demand = watch::Sender::new(wanted(&Showing::default()));
        let gate = Gate::new(demand.subscribe());
        let runs = Arc::new(AtomicUsize::new(0));
        let r = Arc::clone(&runs);
        let task = gated_once(&gate, Source::UpgradeCheck, async move {
            r.fetch_add(1, Ordering::SeqCst);
        });
        run_for(60.0).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0, "not before Updates shows");
        for _ in 0..3 {
            demand.send_replace(wanted(&updates));
            run_for(1.0).await;
            demand.send_replace(wanted(&Showing::tab(Tab::Home)));
            run_for(1.0).await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        task.abort();
    }
}
