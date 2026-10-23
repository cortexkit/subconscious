//! The tasks behind the live layer: the connection supervisor, one task per
//! source, the request handler and the action monitor.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use subc_client_rs::{
    ActivityCursor, ActivityQueryRequest, ActivityQueryResponse, ActivitySubscription, CallError,
    CallOptions, CloseRouteOptions, ConnectionState, ConsumerOptions, RouteHandle,
    SpawnStreamError, SubcConsumer, SubscribeOptions, Subscription,
};
use subc_control::ActivityEntry as WireActivity;
use subc_protocol::{BindIdentity, RouteTarget};
use subc_transport::connection_file;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;

use super::activity::{self, FeedEnd, LedgerFeed, LedgerSource};
use super::map;
use super::pairing::{self, CallFailure, WatchStream};
use super::schedule::{self, Gate, Source, Sources};
use super::{
    action_progress, assemble, every, modules, Connection, DaemonService, HostFacts, LiveOptions,
    LiveProcess, State,
};
use crate::app::Request;
use crate::model::{
    ActionKind, ActionPhase, ActionResult, Fault, ModuleAction, ProcessEvent, ProcessEventKind,
    Snapshot,
};
use crate::pairing::flow::{Command, Refusal};
use crate::time::Timestamp;

/// The longest any one daemon or module request may take before its source
/// counts it as failed and tries again on its next turn.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an action is watched for its outcome before ck reports that it
/// never saw one.
const ACTION_WATCH: Duration = Duration::from_secs(120);

/// How many stderr lines the log view asks for.
const LOG_LINES: u32 = 1000;

struct Shared {
    state: Mutex<State>,
    tx: watch::Sender<Snapshot>,
    service: Arc<dyn DaemonService>,
    conn: Mutex<Option<Conn>>,
    provenance_due: Notify,
    log_due: Notify,
    probe_due: Notify,
    /// Wakes the activity feed when the Activity view moves: an older page
    /// is wanted, or the view is back at the top.
    activity_due: Notify,
    /// Which sources are wanted, from what the app shows.
    demand: watch::Sender<Sources>,
    /// Wakes the agents-place source once identity rows have arrived.
    identity_ready: Notify,
    /// The latest reply or error recorded for each daemon and module
    /// request, for diagnosis.
    raw: Arc<Mutex<BTreeMap<String, String>>>,
}

/// One connection and the module routes opened on it. Routes are opened
/// once and reused; a route the daemon closes is dropped here and opened
/// again on the next call.
#[derive(Clone)]
struct Conn {
    consumer: Arc<SubcConsumer>,
    routes: Arc<tokio::sync::Mutex<HashMap<String, RouteHandle>>>,
}

impl Shared {
    /// Changes the state and sends the snapshot it now describes.
    fn update(&self, f: impl FnOnce(&mut State)) {
        let snap = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut st);
            assemble(&st, Timestamp::system_now())
        };
        self.tx.send_replace(snap);
    }

    fn read<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&st)
    }

    /// Keeps a source's latest raw reply (or error) under its name.
    fn raw(&self, source: &str, text: String) {
        self.raw
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source.to_string(), text);
    }

    fn gate(&self) -> Gate {
        Gate::new(self.demand.subscribe())
    }

    fn conn(&self) -> Option<Conn> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub(super) fn spawn(
    options: LiveOptions,
    state: State,
    tx: watch::Sender<Snapshot>,
    requests: mpsc::UnboundedReceiver<Request>,
    raw: Arc<Mutex<BTreeMap<String, String>>>,
) {
    let shared = Arc::new(Shared {
        state: Mutex::new(state),
        tx,
        service: Arc::clone(&options.service),
        conn: Mutex::new(None),
        provenance_due: Notify::new(),
        log_due: Notify::new(),
        probe_due: Notify::new(),
        activity_due: Notify::new(),
        demand: watch::Sender::new(schedule::wanted(&schedule::Showing::default())),
        identity_ready: Notify::new(),
        raw,
    });
    tokio::spawn(supervise(Arc::clone(&shared), options.connection_file));
    tokio::spawn(handle_requests(Arc::clone(&shared), requests));
    let gate = shared.gate();
    tokio::spawn(host_facts(shared, gate));
}

/// A reply as JSON text, for the raw view.
fn json_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| format!("unprintable: {e}"))
}

fn now() -> Timestamp {
    Timestamp::system_now()
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

// ---- connection ----------------------------------------------------------------

async fn connect(file: Option<&Path>) -> Result<(SubcConsumer, String), String> {
    let discovered = connection_file::discover(file).map_err(|e| e.to_string())?;
    let opts = ConsumerOptions {
        call_timeout: CALL_TIMEOUT,
        handshake_timeout: Duration::from_secs(5),
        ..ConsumerOptions::default()
    };
    let consumer = SubcConsumer::connect(&discovered.path, opts)
        .await
        .map_err(|e| e.to_string())?;
    Ok((consumer, discovered.info.daemon_ver.clone()))
}

/// Connects, runs the sources until the connection drops, and while the
/// daemon is down reads the service's state and tries again every two
/// seconds.
async fn supervise(shared: Arc<Shared>, file: Option<PathBuf>) {
    loop {
        match connect(file.as_deref()).await {
            Ok((consumer, daemon_ver)) => {
                let consumer = Arc::new(consumer);
                let dropped = Arc::new(Notify::new());
                let signal = Arc::clone(&dropped);
                consumer.on_connection_state(move |s| {
                    if matches!(s, ConnectionState::Dropped) {
                        signal.notify_one();
                    }
                });
                let conn = Conn {
                    consumer: Arc::clone(&consumer),
                    routes: Arc::default(),
                };
                *shared.conn.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn.clone());
                shared.update(|s| {
                    s.connection = Connection::Up { daemon_ver };
                    s.connected_at.set((), now());
                    s.down_note = None;
                });
                let tasks = sources(&shared, &conn);
                dropped.notified().await;
                for t in &tasks {
                    t.abort();
                }
                *shared.conn.lock().unwrap_or_else(|e| e.into_inner()) = None;
                consumer.close().await;
                shared.update(|s| {
                    s.connection = Connection::Down {
                        lost_at: Some(now()),
                        reason: "connection lost".into(),
                    };
                    // Nothing in flight can be followed without the daemon.
                    for (id, a) in s.actions.drain() {
                        s.last_result = Some(ActionResult {
                            at: now(),
                            text: format!(
                                "{id}: lost the daemon during the {}; outcome unknown",
                                a.kind.verb()
                            ),
                            ok: false,
                        });
                    }
                });
            }
            Err(reason) => shared.update(|s| {
                let lost_at = match &s.connection {
                    Connection::Down { lost_at, .. } => *lost_at,
                    _ => None,
                };
                s.connection = Connection::Down { lost_at, reason };
            }),
        }
        let service = Arc::clone(&shared.service);
        if let Ok((status, tail)) = tokio::task::spawn_blocking(move || {
            (service.service_status(), service.daemon_log_tail(40))
        })
        .await
        {
            shared.update(|s| {
                s.service = status;
                s.daemon_log = tail;
            });
        }
        tokio::time::sleep(secs(every::RECONNECT)).await;
    }
}

/// The text a failed daemon request shows: the refusal code, or that the
/// daemon isn't answering.
fn control_error(e: &CallError) -> String {
    control_fault(e).text
}

/// A failed daemon request as an error a panel shows: the daemon's refusal
/// code (a machine code), or a sentence saying the daemon isn't answering.
fn control_fault(e: &CallError) -> Fault {
    match e {
        CallError::Module(body) => Fault::code(body.code.clone()),
        _ => match e.code() {
            Some(code) => Fault::code(code),
            None => Fault::sentence("daemon unreachable · retrying"),
        },
    }
}

fn sources(shared: &Arc<Shared>, conn: &Conn) -> Vec<JoinHandle<()>> {
    let mut tasks = Vec::new();
    let gate = shared.gate();
    macro_rules! every {
        ($source:expr, $period:expr, |$sh:ident, $c:ident| $body:expr) => {{
            let sh = Arc::clone(shared);
            let cn = conn.clone();
            tasks.push(schedule::gated_poll(
                &gate,
                $source,
                secs($period),
                move || {
                    let $sh = Arc::clone(&sh);
                    let $c = cn.clone();
                    async move { $body }
                },
            ));
        }};
    }

    // The roster: a CPU percent needs two readings of one process, so the
    // first reading only seeds the CPU series and the second, a second
    // later, is the first one shown. Then every two seconds.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        let mut gate = gate.clone();
        tasks.push(tokio::spawn(async move {
            let mut seeded = false;
            loop {
                gate.until_wanted(Source::Roster).await;
                match c.consumer.supervisor_list().await {
                    Ok(list) => {
                        sh.raw("supervisor.list", json_text(&list.modules));
                        if seeded {
                            sh.update(|s| s.record_roster(list.modules, now()));
                        } else {
                            sh.update(|s| s.record_cpu(&list.modules, now()));
                            seeded = true;
                            tokio::time::sleep(secs(1.0)).await;
                            continue;
                        }
                    }
                    Err(e) => {
                        sh.raw("supervisor.list", control_error(&e));
                        sh.update(|s| s.roster.fail(control_fault(&e)));
                    }
                }
                tokio::time::sleep(secs(every::ROSTER)).await;
            }
        }));
    }
    every!(
        Source::Describe,
        every::DESCRIBE,
        |sh, c| match c.consumer.server_describe().await {
            Ok(d) => {
                sh.raw("server.describe", format!("{d:?}"));
                let reading = super::settings::SettingsReading::read(
                    d.config_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    &d.capabilities,
                );
                sh.update(|s| {
                    s.settings = reading;
                    s.describe.set(d, now());
                })
            }
            Err(e) => {
                sh.raw("server.describe", control_error(&e));
                sh.update(|s| s.describe.fail(control_fault(&e)))
            }
        }
    );
    every!(
        Source::Routes,
        every::ROUTES,
        |sh, c| match c.consumer.supervisor_routes(None).await {
            Ok(r) => {
                sh.raw("supervisor.routes", json_text(&r));
                sh.update(|s| s.routes.set(r, now()))
            }
            Err(e) => {
                sh.raw("supervisor.routes", control_error(&e));
                sh.update(|s| s.routes.fail(control_fault(&e)))
            }
        }
    );
    // Usage, devices, agents' identity and state, and the fleet's counts:
    // one request each, only while a screen that shows them is on screen.
    {
        let (sh, c) = (Arc::clone(shared), conn.clone());
        let call = move |module: &'static str, method: &'static str, params: Value| {
            let (sh, c) = (Arc::clone(&sh), c.clone());
            async move { module_call(&sh, &c, module, method, params).await }
        };
        let sh = Arc::clone(shared);
        let params = move |source: Source| sh.read(|s| s.poll_params(source));
        let sh = Arc::clone(shared);
        let apply = move |source: Source, reply: Result<Value, Fault>| {
            let mut good = false;
            sh.update(|s| good = s.record_module_reply(source, reply, now()));
            if good && source == Source::Identity {
                sh.identity_ready.notify_one();
            }
        };
        tasks.extend(schedule::module_sources(&gate, call, params, apply));
    }
    // Where agents live: as soon as identity rows exist, then every minute,
    // while a screen that shows it is on screen.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        let mut gate = gate.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                gate.until_wanted(Source::Places).await;
                let ids: Vec<String> = sh.read(|s| {
                    s.identities
                        .value
                        .iter()
                        .flatten()
                        .map(|i| i.id.clone())
                        .collect()
                });
                let here = sh.read(|s| s.host_label.clone());
                let (shared, conn) = (&*sh, &c);
                let round = super::where_many_round(&ids, &here, move |params| {
                    module_call(shared, conn, modules::IDENTITY, "agent.where_many", params)
                })
                .await;
                match round {
                    // No identity rows yet: wait for them.
                    None => {
                        let _ =
                            tokio::time::timeout(secs(every::PLACES), sh.identity_ready.notified())
                                .await;
                        continue;
                    }
                    Some(mapped) => sh.update(|s| match mapped {
                        Ok(p) => s.places.set(p, now()),
                        Err(e) => s.places.fail(e),
                    }),
                }
                tokio::select! {
                    _ = tokio::time::sleep(secs(every::PLACES)) => {}
                    _ = gate.until_resumed(Source::Places) => {}
                }
            }
        }));
    }

    // Health: every five seconds, every second while any action is in
    // flight, so a restart's first healthy check shows promptly.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        let mut gate = gate.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                gate.until_wanted(Source::Health).await;
                match c.consumer.supervisor_health().await {
                    Ok(h) => sh.update(|s| {
                        sh.raw("supervisor.health", json_text(&h.modules));
                        s.health.set(h.modules, now());
                        record_build_sample(s);
                    }),
                    Err(e) => {
                        sh.raw("supervisor.health", control_error(&e));
                        sh.update(|s| s.health.fail(control_fault(&e)))
                    }
                }
                let busy = sh.read(|s| !s.actions.is_empty());
                tokio::time::sleep(secs(if busy { 1.0 } else { every::HEALTH })).await;
            }
        }));
    }
    // Provenance: now, after every spawn event, and once a minute.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        let mut gate = gate.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                gate.until_wanted(Source::Provenance).await;
                match c.consumer.supervisor_provenance(None).await {
                    Ok(p) => {
                        sh.raw("supervisor.provenance", json_text(&p.modules));
                        sh.update(|s| s.provenance.set(p, now()))
                    }
                    Err(e) => {
                        sh.raw("supervisor.provenance", control_error(&e));
                        sh.update(|s| s.provenance.fail(control_fault(&e)))
                    }
                }
                let _ = tokio::time::timeout(secs(every::PROVENANCE), sh.provenance_due.notified())
                    .await;
            }
        }));
    }
    // The log on screen. A module's log files are opened once (their newest
    // lines) and then followed: each tick reads what was appended since the
    // last (and a file's first bytes, to notice a rotation), never a whole
    // file again. A program with no file-log reader falls back to the stderr
    // the daemon keeps in memory. Ticks every two seconds, and at once when
    // the app switches to another module's log.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        let mut gate = gate.clone();
        tasks.push(tokio::spawn(async move {
            let mut open: Option<(String, Box<dyn super::ModuleLogFollow>)> = None;
            loop {
                gate.until_wanted(Source::Log).await;
                match sh.read(|s| s.focus.clone()) {
                    None => open = None,
                    Some(module) => {
                        let following = open.take().filter(|(m, _)| *m == module);
                        match following {
                            Some((m, follow)) => {
                                open = follow_log(&sh, m, follow).await;
                            }
                            None => {
                                open = open_log(&sh, &c, module).await;
                            }
                        }
                    }
                }
                let _ = tokio::time::timeout(secs(every::LOG), sh.log_due.notified()).await;
            }
        }));
    }
    // The focused module's health probe: complete metrics for its pane,
    // every five seconds while it is on screen and at once when it changes.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        let mut gate = gate.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                gate.until_wanted(Source::Health).await;
                if let Some(module) = sh.read(|s| s.focus.clone()) {
                    match c.consumer.supervisor_health_probe(module.clone()).await {
                        Ok(p) => {
                            sh.raw("supervisor.health_probe", format!("{p:?}"));
                            sh.update(|s| record_probe(s, module, p));
                        }
                        Err(e) => {
                            sh.raw("supervisor.health_probe", control_error(&e));
                        }
                    }
                }
                let _ = tokio::time::timeout(secs(every::HEALTH), sh.probe_due.notified()).await;
            }
        }));
    }
    // Modules whose cached metrics are over the cache's size cap: checked
    // every five seconds, and probed directly for the full set when their
    // last probe (or failed attempt) is 30 s old.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        let mut gate = gate.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                gate.until_wanted(Source::Health).await;
                for module in sh.read(|s| s.truncated_due(now())) {
                    match c.consumer.supervisor_health_probe(module.clone()).await {
                        Ok(p) => {
                            sh.raw(
                                &format!("supervisor.health_probe {module}"),
                                format!("{p:?}"),
                            );
                            sh.update(|s| record_probe(s, module, p));
                        }
                        Err(e) => {
                            sh.raw(
                                &format!("supervisor.health_probe {module}"),
                                control_error(&e),
                            );
                            // A failed attempt counts as a probe for timing, so
                            // the module isn't asked again before the interval
                            // is up; its last good metrics stay.
                            sh.update(|s| {
                                s.probes
                                    .entry(module)
                                    .and_modify(|p| p.at = now())
                                    .or_insert(super::Probe {
                                        at: now(),
                                        detail: None,
                                        metrics: None,
                                    });
                            });
                        }
                    }
                }
                tokio::time::sleep(secs(every::HEALTH)).await;
            }
        }));
    }
    // Each module's recent process exits, as the daemon's terminal journal
    // retains them, for the "Starts and exits" list on the Modules screen.
    every!(Source::Terminals, every::TERMINALS, |sh, c| {
        let ids: Vec<String> = sh.read(|s| {
            s.roster
                .value
                .iter()
                .flatten()
                .map(|e| e.module_id.clone())
                .collect()
        });
        for id in ids {
            if let Ok(history) = c.consumer.supervisor_terminals(id.clone()).await {
                let events = map::terminal_events(&id, &history);
                sh.update(|s| {
                    s.terminals.insert(id, events);
                });
            }
        }
    });
    {
        let (sh, c) = (Arc::clone(shared), conn.clone());
        tasks.push(schedule::gated_session(
            &gate,
            Source::SpawnFeed,
            move || spawn_feed(Arc::clone(&sh), c.clone()),
        ));
    }
    {
        let sh = Arc::clone(shared);
        let ledger: Arc<dyn LedgerSource> = Arc::new(DaemonLedger(Arc::clone(&conn.consumer)));
        // Where the feed resumes is kept in the state, across sessions and
        // connections, so a feed paused while no screen shows it resumes
        // from its last head.
        tasks.push(schedule::gated_session(
            &gate,
            Source::Activity,
            move || activity_session(Arc::clone(&sh), Arc::clone(&ledger)),
        ));
    }
    tasks
}

fn record_probe(s: &mut State, module: String, p: subc_client_rs::HealthProbeResponse) {
    s.probes.insert(
        module,
        super::Probe {
            at: now(),
            detail: p.detail,
            metrics: p.metrics,
        },
    );
    record_build_sample(s);
}

/// The most lines the log view keeps; older ones are dropped as new ones
/// arrive.
const LOG_KEEP: usize = 2000;

/// Opens `module`'s log: its files when this program can read them (the
/// newest lines, then a follower), else the daemon's in-memory stderr. A
/// failure to read the files shows as the log's error.
async fn open_log(
    sh: &Arc<Shared>,
    c: &Conn,
    module: String,
) -> Option<(String, Box<dyn super::ModuleLogFollow>)> {
    let service = Arc::clone(&sh.service);
    let id = module.clone();
    let opened =
        tokio::task::spawn_blocking(move || service.open_module_log(&id, LOG_LINES as usize))
            .await
            .unwrap_or_else(|_| Some(Err("the log reader stopped".into())));
    match opened {
        Some(Ok(opened)) => {
            let lines = map::merged_log_lines(&opened.lines);
            sh.update(|s| {
                s.log.set((module.clone(), lines), now());
                s.log_source = "{module}'s log files, captured stderr and daemon lines".into();
            });
            Some((module, opened.follow))
        }
        Some(Err(e)) => {
            sh.update(|s| {
                s.log = super::Part::default();
                s.log.fail(e);
            });
            None
        }
        None => {
            match c
                .consumer
                .supervisor_stderr_tail(module.clone(), Some(LOG_LINES), None)
                .await
            {
                Ok(t) => {
                    let lines = map::log_lines(&t.tail.entries);
                    sh.update(|s| {
                        s.log.set((module, lines), now());
                        s.log_source = "{module}'s stderr, as the daemon keeps it".into();
                    });
                }
                Err(e) => sh.update(|s| {
                    s.log = super::Part::default();
                    s.log.fail(control_fault(&e));
                }),
            }
            None
        }
    }
}

/// One follow tick: the lines appended since the last, added to the log.
/// A failed read shows as the log's error and following goes on.
async fn follow_log(
    sh: &Arc<Shared>,
    module: String,
    mut follow: Box<dyn super::ModuleLogFollow>,
) -> Option<(String, Box<dyn super::ModuleLogFollow>)> {
    let polled = tokio::task::spawn_blocking(move || {
        let new = follow.poll();
        (follow, new)
    })
    .await;
    let Ok((follow, new)) = polled else {
        sh.update(|s| s.log.fail("the log reader stopped"));
        return None;
    };
    match new {
        Ok(new) => {
            let new = map::merged_log_lines(&new);
            sh.update(|s| {
                if let Some((m, lines)) = s.log.value.as_mut() {
                    if *m == module {
                        lines.extend(new);
                        let excess = lines.len().saturating_sub(LOG_KEEP);
                        lines.drain(..excess);
                    }
                }
                s.log.at = now();
                s.log.error = None;
            });
        }
        Err(e) => sh.update(|s| s.log.fail(e)),
    }
    Some((module, follow))
}

/// Keeps ck's own build-server load samples, for motor builds that send no
/// history: at most one a minute, and the last 180 of them, the same span
/// motor's own history covers.
fn record_build_sample(s: &mut State) {
    let Some(m) = s.metrics_of(modules::BUILD_SERVER).cloned() else {
        return;
    };
    let busy = map::metric(&m, "build_server.permits_held").and_then(Value::as_f64);
    let cap = map::metric(&m, "build_server.permits_cap").and_then(Value::as_f64);
    let at = Timestamp::system_now();
    if s.build_sample_at.is_some_and(|t| at.secs_since(t) < 60.0) {
        return;
    }
    if let (Some(b), Some(c)) = (busy, cap) {
        if c > 0.0 {
            s.build_sample_at = Some(at);
            s.build_samples.push(b / c * 100.0);
            let excess = s.build_samples.len().saturating_sub(180);
            s.build_samples.drain(..excess);
        }
    }
}

// ---- module calls --------------------------------------------------------------

fn call_options() -> CallOptions {
    CallOptions {
        timeout: CALL_TIMEOUT,
        route_retry_deadline: Duration::from_secs(5),
        ..CallOptions::default()
    }
}

/// Whether a failed call means the route itself is gone, so the next call
/// should open a new one.
fn route_gone(e: &CallError) -> bool {
    matches!(e, CallError::StaleRouteHandle(_))
        || e.close_reason().is_some()
        || matches!(
            e.code(),
            Some("unknown_channel" | "stale_route_epoch" | "module_reloading" | "route_closed")
        )
}

/// One `{"method", "params"}` request to a module's management surface, on
/// the route kept for that module. A route the daemon has closed is reopened
/// once. Errors are one line: the module's refusal code, "not installed",
/// "unreachable · retrying", or that the reply couldn't be read.
async fn module_call(
    shared: &Shared,
    conn: &Conn,
    module: &str,
    method: &str,
    params: Value,
) -> Result<Value, Fault> {
    match module_request(shared, conn, module, method, params, call_options(), true).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| Fault::sentence(format!("{module} sent an unreadable reply"))),
        Err(ModuleFailure::NotInstalled) => {
            Err(Fault::sentence(format!("{module} isn't installed")))
        }
        Err(ModuleFailure::Call(e)) => Err(module_error(module, &e)),
        Err(ModuleFailure::Unreachable) => {
            Err(Fault::sentence(format!("{module} unreachable · retrying")))
        }
    }
}

/// Why a module request got no reply.
enum ModuleFailure {
    NotInstalled,
    Call(CallError),
    Unreachable,
}

/// The request behind [`module_call`], returning the reply's bytes or the
/// call's own error. With `keep_raw` the reply is kept for diagnosis; a
/// reply that carries a secret, such as a pairing code, is not.
async fn module_request(
    shared: &Shared,
    conn: &Conn,
    module: &str,
    method: &str,
    params: Value,
    options: CallOptions,
    keep_raw: bool,
) -> Result<Vec<u8>, ModuleFailure> {
    if shared.read(|s| s.in_roster(module)) == Some(false) {
        return Err(ModuleFailure::NotInstalled);
    }
    let body = serde_json::to_vec(&json!({ "method": method, "params": params }))
        .map_err(|_| ModuleFailure::Unreachable)?;
    for attempt in 0..2 {
        let handle = {
            let mut routes = conn.routes.lock().await;
            match routes.get(module) {
                Some(h) => *h,
                None => {
                    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
                    let h = conn
                        .consumer
                        .open_route(
                            RouteTarget::ManagementSurface {
                                module_id: module.to_string(),
                            },
                            BindIdentity::new(root, "ck", "app"),
                            call_options(),
                        )
                        .await
                        .map_err(ModuleFailure::Call)?;
                    routes.insert(module.to_string(), h);
                    h
                }
            }
        };
        match conn
            .consumer
            .request(&handle, body.clone(), options.clone())
            .await
        {
            Ok(bytes) => {
                if keep_raw {
                    shared.raw(
                        &format!("{module} {method}"),
                        String::from_utf8_lossy(&bytes).into_owned(),
                    );
                }
                return Ok(bytes);
            }
            Err(e) => {
                shared.raw(&format!("{module} {method}"), format!("error: {e}"));
                if route_gone(&e) {
                    conn.routes.lock().await.remove(module);
                    if attempt == 0 {
                        continue;
                    }
                }
                return Err(ModuleFailure::Call(e));
            }
        }
    }
    Err(ModuleFailure::Unreachable)
}

/// A failed module request as an error a panel shows: the module's or the
/// daemon's refusal code (a machine code), or a sentence saying the module
/// isn't answering.
fn module_error(module: &str, e: &CallError) -> Fault {
    match e {
        CallError::Module(body) => Fault::code(body.code.clone()),
        _ => match e.route_open_refusal() {
            Some(body) => Fault::code(body.code.clone()),
            None => Fault::sentence(format!("{module} unreachable · retrying")),
        },
    }
}

// ---- subscriptions ---------------------------------------------------------------

/// Marks a subscription closed when its task ends, however it ends: a
/// session dropped because no screen shows its feed, or aborted with the
/// connection, never ran the code after its last await.
struct MarkClosedOnDrop {
    shared: Arc<Shared>,
    close: fn(&mut State),
}

impl Drop for MarkClosedOnDrop {
    fn drop(&mut self) {
        self.shared.update(self.close);
    }
}

/// Live processes: seeded from the spawn snapshot, then followed. After a
/// lag the feed resubscribes from the last cursor it saw; a cursor the
/// daemon no longer accepts reseeds from a fresh snapshot.
async fn spawn_feed(shared: Arc<Shared>, conn: Conn) {
    let _closed = MarkClosedOnDrop {
        shared: Arc::clone(&shared),
        close: |s| s.spawn_open = false,
    };
    let mut cursor = None;
    loop {
        if cursor.is_none() {
            match conn.consumer.spawn_snapshot().await {
                Ok(snap) => {
                    shared.update(|s| {
                        s.processes = snap
                            .live
                            .iter()
                            .map(|l| {
                                (
                                    l.module_id.clone(),
                                    LiveProcess {
                                        pid: l.pid,
                                        generation: l.spawn_generation,
                                        spawned_at: Timestamp::from_unix_millis(
                                            l.spawned_at_ms as i64,
                                        ),
                                    },
                                )
                            })
                            .collect();
                        s.spawn_feed.set((), now());
                    });
                    cursor = Some(snap.cursor);
                }
                Err(e) => {
                    shared.update(|s| s.spawn_feed.fail(control_fault(&e)));
                    tokio::time::sleep(secs(2.0)).await;
                    continue;
                }
            }
        }
        let mut sub = match conn.consumer.spawn_subscribe(cursor.clone()).await {
            Ok(sub) => sub,
            Err(e) => {
                shared.update(|s| s.spawn_feed.fail(control_fault(&e)));
                tokio::time::sleep(secs(2.0)).await;
                continue;
            }
        };
        // The subscription opening counts as hearing from the feed: an open
        // feed has nothing to report until something happens.
        shared.update(|s| {
            s.spawn_open = true;
            s.spawn_feed.at = now();
            s.spawn_feed.error = None;
        });
        loop {
            match sub.next().await {
                Ok(Some(ev)) => {
                    cursor = Some(ev.cursor.clone());
                    shared.update(|s| {
                        on_spawn_event(s, &ev, now());
                        s.spawn_feed.set((), now());
                    });
                    shared.provenance_due.notify_one();
                }
                Ok(None) | Err(SpawnStreamError::SubscriberLagged { .. }) => break,
                Err(
                    SpawnStreamError::CursorTooOld { .. }
                    | SpawnStreamError::CursorIncarnationMismatch { .. },
                ) => {
                    cursor = None;
                    break;
                }
                Err(e) => {
                    shared.update(|s| {
                        s.spawn_open = false;
                        s.spawn_feed.fail(e.to_string());
                    });
                    tokio::time::sleep(secs(2.0)).await;
                    break;
                }
            }
        }
        shared.update(|s| s.spawn_open = false);
    }
}

/// Applies one spawn event: the module's live process, its exit, and a line
/// in the starts and exits list.
pub(super) fn on_spawn_event(s: &mut State, ev: &subc_control::SpawnEvent, at: Timestamp) {
    let (what, kind) = match ev.kind {
        subc_control::SpawnEventKind::Spawned => {
            s.processes.insert(
                ev.module_id.clone(),
                LiveProcess {
                    pid: ev.pid,
                    generation: ev.spawn_generation,
                    spawned_at: at,
                },
            );
            (
                format!("started · pid {}", ev.pid),
                ProcessEventKind::Started,
            )
        }
        subc_control::SpawnEventKind::Exited => {
            if s.processes.get(&ev.module_id).map(|p| p.pid) == Some(ev.pid) {
                s.processes.remove(&ev.module_id);
            }
            s.exits.insert(ev.module_id.clone(), at);
            let how = match (ev.exit_code, ev.exit_signal) {
                (Some(0), _) => "exited cleanly".to_string(),
                (Some(code), _) => format!("exited with code {code}"),
                (None, Some(sig)) => format!("exited on signal {sig}"),
                (None, None) => "exited".to_string(),
            };
            (format!("{how} · pid {}", ev.pid), ProcessEventKind::Exited)
        }
    };
    s.process_events.insert(
        0,
        ProcessEvent {
            at,
            module: ev.module_id.clone(),
            what,
            kind,
        },
    );
    s.process_events.truncate(200);
}

/// One session of the activity ledger, run while a screen shows it.
///
/// With nowhere to resume from it reads the newest page, then subscribes from
/// that page's head. Otherwise it subscribes from the last head the feed
/// reached, so entries recorded while no screen showed the ledger replay in
/// order. When the daemon refuses that head (another daemon incarnation, or
/// entries after it have left the replay ring), it reads the newest pages
/// down to what is loaded, then subscribes from the newest page's head. A
/// lag resubscribes from the last head. While subscribed it also reads what
/// the Activity view asks for: an older page, or the newest entries again
/// once the view is back at the top.
async fn activity_session(shared: Arc<Shared>, ledger: Arc<dyn LedgerSource>) {
    let _closed = MarkClosedOnDrop {
        shared: Arc::clone(&shared),
        close: |s| s.ledger.open = false,
    };
    let mut refill = shared.read(|s| s.ledger.head.is_none());
    loop {
        if refill {
            if let Err(e) = refill_top(&shared, &*ledger, true).await {
                shared.update(|s| s.activity.fail(e));
                tokio::time::sleep(secs(5.0)).await;
                return;
            }
        }
        let since = shared.read(|s| s.ledger.head.clone());
        let mut feed = match ledger.subscribe(since).await {
            Ok(feed) => feed,
            Err(FeedEnd::CursorRefused) => {
                refill = true;
                continue;
            }
            Err(FeedEnd::Failed(e)) => {
                shared.update(|s| s.activity.fail(e));
                tokio::time::sleep(secs(5.0)).await;
                return;
            }
            Err(FeedEnd::Ended | FeedEnd::Lagged) => {
                tokio::time::sleep(secs(1.0)).await;
                return;
            }
        };
        shared.update(|s| s.record_feed_open(now()));
        refill = false;
        let end = loop {
            tokio::select! {
                next = feed.next() => match next {
                    Ok(entry) => shared.update(|s| s.record_live_activity(entry, now())),
                    Err(end) => break end,
                },
                _ = shared.activity_due.notified() => {
                    if let Err(e) = serve_activity_view(&shared, &*ledger).await {
                        shared.update(|s| s.activity.fail(e));
                    }
                }
            }
        };
        shared.update(|s| s.ledger.open = false);
        match end {
            FeedEnd::Lagged => {}
            FeedEnd::CursorRefused => refill = true,
            FeedEnd::Ended => {
                tokio::time::sleep(secs(1.0)).await;
                return;
            }
            FeedEnd::Failed(e) => {
                shared.update(|s| s.activity.fail(e));
                tokio::time::sleep(secs(2.0)).await;
                return;
            }
        }
    }
}

/// Reads the newest pages until they reach an entry already loaded, or
/// history ends (a label then marks the entries no longer kept). With
/// nothing loaded that is one page. `take_head` makes the newest page's head
/// the feed's head, for a feed that must subscribe again from it.
async fn refill_top(
    shared: &Shared,
    ledger: &dyn LedgerSource,
    take_head: bool,
) -> Result<(), Fault> {
    let mut refill = shared.read(|s| s.ledger.begin_refill(take_head));
    let mut cursor = None;
    loop {
        let page = ledger.query(cursor).await?;
        let mut next = None;
        shared.update(|s| next = s.record_refill_page(&mut refill, page, now()));
        match next {
            Some(c) => cursor = Some(c),
            None => return Ok(()),
        }
    }
}

/// Reads what the Activity view asked for: the newest entries again when
/// they were dropped and the view is back at the top, and the next older
/// page when the view reached the oldest loaded row.
async fn serve_activity_view(shared: &Shared, ledger: &dyn LedgerSource) -> Result<(), Fault> {
    if shared.read(State::top_refill_wanted) {
        refill_top(shared, ledger, false).await?;
    }
    let mut older = None;
    shared.update(|s| older = s.take_older_wanted());
    if let Some(cursor) = older {
        let page = ledger.query(Some(cursor)).await?;
        shared.update(|s| s.record_older_page(page, now()));
    }
    Ok(())
}

/// The daemon's activity ledger.
struct DaemonLedger(Arc<SubcConsumer>);

impl LedgerSource for DaemonLedger {
    fn query(&self, cursor: Option<u64>) -> BoxFuture<'_, Result<ActivityQueryResponse, Fault>> {
        Box::pin(async move {
            let query = ActivityQueryRequest {
                agent: None,
                project: None,
                module: None,
                kind: None,
                since_ms: None,
                until_ms: None,
                limit: activity::PAGE,
                cursor,
            };
            self.0
                .query_activity(query)
                .await
                .map_err(|e| control_fault(&e))
        })
    }

    fn subscribe(
        &self,
        since: Option<ActivityCursor>,
    ) -> BoxFuture<'_, Result<Box<dyn LedgerFeed>, FeedEnd>> {
        Box::pin(async move {
            match self.0.subscribe_activity(since).await {
                Ok(sub) => Ok(Box::new(DaemonFeed(sub)) as Box<dyn LedgerFeed>),
                Err(e) => Err(FeedEnd::from_code(e.code(), control_fault(&e))),
            }
        })
    }
}

/// The daemon's activity subscription.
struct DaemonFeed(ActivitySubscription);

impl LedgerFeed for DaemonFeed {
    fn next(&mut self) -> BoxFuture<'_, Result<WireActivity, FeedEnd>> {
        Box::pin(async move {
            match self.0.next().await {
                Ok(Some(entry)) => Ok(entry),
                Ok(None) => Err(FeedEnd::Ended),
                Err(e) => {
                    let fault = match e.code() {
                        Some(code) => Fault::code(code),
                        None => Fault::sentence(e.to_string()),
                    };
                    Err(FeedEnd::from_code(e.code(), fault))
                }
            }
        })
    }
}

// ---- host ------------------------------------------------------------------------

/// The machine's memory, load and data-home volume, read locally every ten
/// seconds whether or not the daemon is up.
async fn host_facts(shared: Arc<Shared>, mut gate: Gate) {
    let service = Arc::clone(&shared.service);
    let mut tick = tokio::time::interval(secs(every::HOST));
    loop {
        tick.tick().await;
        gate.until_wanted(Source::Host).await;
        let service = Arc::clone(&service);
        let read = tokio::task::spawn_blocking(move || {
            let home = service.data_home();
            HostFacts {
                memory: subc_os::host::memory(),
                load: subc_os::host::load_average(),
                disk: home
                    .as_deref()
                    .and_then(existing_ancestor)
                    .and_then(|p| subc_os::host::disk_space(&p)),
            }
        })
        .await;
        if let Ok(facts) = read {
            shared.update(|s| s.host.set(facts, now()));
        }
    }
}

/// `path`, or its nearest ancestor that exists: the volume a data home will
/// live on before it is created.
fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|p| p.exists()).map(Path::to_path_buf)
}

// ---- requests --------------------------------------------------------------------

async fn handle_requests(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Request>) {
    let pairing = {
        let sink = Arc::clone(&shared);
        pairing::Pairing::new(
            Arc::new(DaemonCallosum(Arc::clone(&shared))),
            Arc::new(move |event| sink.update(|s| s.record_pairing(event))),
            now,
        )
    };
    while let Some(req) = rx.recv().await {
        match req {
            Request::Pairing(Command::ReadGroups) => {
                tokio::spawn(read_groups(Arc::clone(&shared)));
            }
            Request::Pairing(Command::Record { text, ok }) => shared.update(|s| {
                s.last_result = Some(ActionResult {
                    at: now(),
                    text,
                    ok,
                })
            }),
            Request::Pairing(command) => pairing.run(command),
            Request::Show(showing) => {
                let wanted = schedule::wanted(&showing);
                shared.demand.send_if_modified(|d| {
                    let changed = *d != wanted;
                    *d = wanted;
                    changed
                });
                // Away from Activity the ledger is read from the top, as
                // Home and Modules show it.
                if showing.tab != crate::model::Tab::Activity {
                    shared.update(|s| s.ledger.anchor = None);
                    shared.activity_due.notify_one();
                }
            }
            Request::ActivityView { anchor, older } => {
                shared.update(|s| s.set_activity_view(anchor, older));
                shared.activity_due.notify_one();
            }
            Request::WatchLog(module) => {
                shared.update(|s| s.focus = module);
                shared.log_due.notify_one();
                shared.probe_due.notify_one();
            }
            Request::StartDaemon => {
                tokio::spawn(start_daemon(Arc::clone(&shared)));
            }
            Request::Act { kind, module } => {
                tokio::spawn(run_action(Arc::clone(&shared), kind, module));
            }
        }
    }
}

/// Reads `callosum.devices_read` now, for the Add a phone overlay's access
/// groups, whatever the devices source's schedule.
async fn read_groups(shared: Arc<Shared>) {
    let reply = match shared.conn() {
        Some(conn) => {
            module_call(
                &shared,
                &conn,
                modules::DEVICES,
                "callosum.devices_read",
                json!({}),
            )
            .await
        }
        None => Err(Fault::sentence("daemon unreachable · retrying")),
    };
    shared.update(|s| {
        s.record_module_reply(Source::Devices, reply, now());
    });
}

// ---- pairing ---------------------------------------------------------------------

/// Callosum on the connected daemon, for the Add a phone exchange.
struct DaemonCallosum(Arc<Shared>);

/// A failed callosum call as the exchange reads it: a refusal keeps its code,
/// message and named wait; anything else leaves the outcome unknown.
fn call_failure(e: &CallError) -> CallFailure {
    let body = match e {
        CallError::Module(body) => Some(body),
        _ => e.route_open_refusal(),
    };
    match body {
        Some(body) => CallFailure::Refused(Refusal {
            code: body.code.clone(),
            message: body.message.clone(),
            retry_after_ms: body
                .detail
                .as_ref()
                .and_then(|d| d.get("retry_after_ms"))
                .and_then(Value::as_u64),
        }),
        None => CallFailure::Lost,
    }
}

impl pairing::Callosum for DaemonCallosum {
    fn call(
        &self,
        method: &'static str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'static, Result<Value, CallFailure>> {
        let shared = Arc::clone(&self.0);
        Box::pin(async move {
            let Some(conn) = shared.conn() else {
                return Err(CallFailure::Lost);
            };
            let options = CallOptions {
                timeout,
                ..call_options()
            };
            // The open's reply carries the pairing secret, so no pairing
            // reply is kept for diagnosis.
            let reply = module_request(
                &shared,
                &conn,
                modules::DEVICES,
                method,
                params,
                options,
                false,
            )
            .await;
            match reply {
                // A reply that isn't JSON reads as one without the expected
                // fields.
                Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null)),
                Err(ModuleFailure::NotInstalled) => Err(CallFailure::Refused(Refusal::code(
                    "callosum isn't installed",
                ))),
                Err(ModuleFailure::Call(e)) => Err(call_failure(&e)),
                Err(ModuleFailure::Unreachable) => Err(CallFailure::Lost),
            }
        })
    }

    fn watch(
        &self,
        window_id: String,
    ) -> BoxFuture<'static, Result<Box<dyn WatchStream>, CallFailure>> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let shared = Arc::clone(&self.0);
        Box::pin(async move {
            let Some(conn) = shared.conn() else {
                return Err(CallFailure::Lost);
            };
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            // A session name of its own gives the watch a route of its own,
            // apart from the shared route opens and confirms use.
            let session = format!(
                "pairing-watch-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let handle = conn
                .consumer
                .open_route(
                    RouteTarget::ManagementSurface {
                        module_id: modules::DEVICES.to_string(),
                    },
                    BindIdentity::new(root, "ck", session),
                    call_options(),
                )
                .await
                .map_err(|e| call_failure(&e))?;
            let body = serde_json::to_vec(&json!({
                "method": "callosum.pairing_qr_watch",
                "params": { "window_id": window_id },
            }))
            .map_err(|_| CallFailure::Lost)?;
            match conn
                .consumer
                .subscribe_route(&handle, body, SubscribeOptions::default())
                .await
            {
                Ok(sub) => Ok(Box::new(DaemonWatch {
                    consumer: Arc::clone(&conn.consumer),
                    handle,
                    sub,
                }) as Box<dyn WatchStream>),
                Err(e) => {
                    let _ = conn
                        .consumer
                        .close_handle(&handle, CloseRouteOptions::default())
                        .await;
                    Err(call_failure(&e))
                }
            }
        })
    }
}

/// One window's watch subscription and the route it alone uses.
struct DaemonWatch {
    consumer: Arc<SubcConsumer>,
    handle: RouteHandle,
    sub: Subscription,
}

impl WatchStream for DaemonWatch {
    fn next(&mut self) -> BoxFuture<'_, Option<Value>> {
        Box::pin(async move {
            // Each notice is one stream event; after a terminal state the
            // stream ends. Bytes that aren't JSON come through as `null`,
            // which the parser reports as unreadable.
            let bytes = self.sub.events().recv().await?;
            Some(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        })
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let _ = self.sub.unsubscribe();
            let _ = self
                .consumer
                .close_handle(&self.handle, CloseRouteOptions::default())
                .await;
        })
    }
}

async fn start_daemon(shared: Arc<Shared>) {
    shared.update(|s| s.down_note = Some("Starting the service…".into()));
    let service = Arc::clone(&shared.service);
    let result = tokio::task::spawn_blocking(move || service.start_service()).await;
    let note = match result {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => format!("Couldn't start it: {e}"),
        Err(_) => "Couldn't start it: the attempt didn't finish".into(),
    };
    shared.update(|s| s.down_note = Some(note));
}

/// Sends one lifecycle request and follows it until the daemon shows its
/// outcome, updating the module's phase as spawn events and health arrive.
async fn run_action(shared: Arc<Shared>, kind: ActionKind, module: String) {
    let started = now();
    let finish = |ok: bool, text: String| {
        shared.update(|s| {
            s.actions.remove(&module);
            s.last_result = Some(ActionResult {
                at: now(),
                text,
                ok,
            });
        });
    };
    let Some(conn) = shared.conn() else {
        finish(false, format!("{module}: the daemon isn't connected"));
        return;
    };
    let before = shared.read(|s| s.processes.get(&module).copied());
    shared.update(|s| {
        s.actions.insert(
            module.clone(),
            ModuleAction {
                kind,
                phase: ActionPhase::Asking,
                started_at: started,
            },
        );
    });
    let reply = match kind {
        ActionKind::Restart => conn.consumer.supervisor_restart(module.clone(), None).await,
        ActionKind::Stop => {
            conn.consumer
                .supervisor_set_enabled(module.clone(), false)
                .await
        }
        ActionKind::Start => {
            conn.consumer
                .supervisor_set_enabled(module.clone(), true)
                .await
        }
    };
    match reply {
        Err(e) => {
            let why = match &e {
                CallError::Module(body) if !body.message.is_empty() => {
                    format!("{} ({})", body.code, body.message)
                }
                _ => control_error(&e),
            };
            finish(false, format!("{module}: {} refused: {why}", kind.verb()));
            return;
        }
        Ok(ack) if !ack.applied => {
            let text = match kind {
                ActionKind::Stop => format!("{module} was already stopped"),
                ActionKind::Start => format!("{module} was already running"),
                ActionKind::Restart => format!("{module}: the daemon didn't restart it"),
            };
            finish(true, text);
            return;
        }
        Ok(_) => {}
    }
    let deadline = tokio::time::Instant::now() + ACTION_WATCH;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (phase, done) =
            shared.read(|s| action_progress(s, kind, &module, before, started, now()));
        if let Some((ok, text)) = done {
            finish(ok, text);
            return;
        }
        shared.update(|s| {
            if let Some(a) = s.actions.get_mut(&module) {
                a.phase = phase;
            }
        });
        if tokio::time::Instant::now() >= deadline {
            finish(
                false,
                format!(
                    "{module}: no outcome seen within {} s of the {}",
                    ACTION_WATCH.as_secs(),
                    kind.verb()
                ),
            );
            return;
        }
    }
}

#[cfg(test)]
#[path = "activity_session_tests.rs"]
mod activity_session_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::schedule::Showing;
    use crate::live::NoService;
    use crate::model::Tab;

    #[tokio::test]
    async fn what_the_app_shows_decides_the_sources_wanted() {
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
            demand: watch::Sender::new(schedule::wanted(&Showing::default())),
            identity_ready: Notify::new(),
            raw: Arc::default(),
        });
        let mut gate = shared.gate();
        assert!(
            gate.wanted(Source::Devices),
            "Home, before the app says anything"
        );
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(handle_requests(Arc::clone(&shared), req_rx));

        req_tx
            .send(Request::Show(Showing::tab(Tab::Usage)))
            .unwrap();
        tokio::time::timeout(secs(5.0), gate.until_unwanted(Source::Devices))
            .await
            .expect("devices stop being wanted on Usage");
        assert_eq!(
            *shared.demand.borrow(),
            schedule::wanted(&Showing::tab(Tab::Usage))
        );
        task.abort();
    }
}
