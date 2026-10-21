//! The tasks behind the live layer: the connection supervisor, one task per
//! source, the request handler and the action monitor.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use subc_client_rs::{
    ActivityCursor, ActivityQueryRequest, CallError, CallOptions, ConnectionState, ConsumerOptions,
    RouteHandle, SpawnStreamError, SubcConsumer,
};
use subc_protocol::{BindIdentity, RouteTarget};
use subc_transport::connection_file;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;

use super::map;
use super::{
    action_progress, assemble, every, modules, Connection, DaemonService, HostFacts, LiveOptions,
    LiveProcess, State,
};
use crate::app::Request;
use crate::model::{
    ActionKind, ActionPhase, ActionResult, Fault, ModuleAction, ProcessEvent, ProcessEventKind,
    Snapshot,
};
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
        identity_ready: Notify::new(),
        raw,
    });
    tokio::spawn(supervise(Arc::clone(&shared), options.connection_file));
    tokio::spawn(handle_requests(Arc::clone(&shared), requests));
    tokio::spawn(host_facts(shared));
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

/// Runs `f` every `period`, starting now.
fn poll<F, Fut>(period: f64, mut f: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(secs(period));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            f().await;
        }
    })
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
    macro_rules! every {
        ($period:expr, |$sh:ident, $c:ident| $body:expr) => {{
            let sh = Arc::clone(shared);
            let cn = conn.clone();
            tasks.push(poll($period, move || {
                let $sh = Arc::clone(&sh);
                let $c = cn.clone();
                async move { $body }
            }));
        }};
    }

    // The roster: a CPU percent needs two readings of one process, so the
    // first reading only seeds the CPU series and the second, a second
    // later, is the first one shown. Then every two seconds.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        tasks.push(tokio::spawn(async move {
            let mut seeded = false;
            loop {
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
        every::DESCRIBE,
        |sh, c| match c.consumer.server_describe().await {
            Ok(d) => {
                sh.raw("server.describe", format!("{d:?}"));
                sh.update(|s| s.describe.set(d, now()))
            }
            Err(e) => {
                sh.raw("server.describe", control_error(&e));
                sh.update(|s| s.describe.fail(control_fault(&e)))
            }
        }
    );
    every!(
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
    every!(every::USAGE, |sh, c| {
        let reply = module_call(&sh, &c, modules::USAGE, "usage.get", json!({})).await;
        let mapped = reply.and_then(|v| map::usage(&v, now()));
        sh.update(|s| match mapped {
            Ok(p) => s.usage.set(p, now()),
            Err(e) => s.usage.fail(e),
        });
    });
    every!(every::DEVICES, |sh, c| {
        let reply = module_call(
            &sh,
            &c,
            modules::DEVICES,
            "callosum.devices_read",
            json!({}),
        )
        .await;
        let mapped = reply.and_then(|v| map::devices(&v, now()));
        sh.update(|s| match mapped {
            Ok(d) => s.devices.set(d, now()),
            Err(e) => s.devices.fail(e),
        });
    });
    every!(every::AGENTS, |sh, c| {
        let reply = module_call(
            &sh,
            &c,
            modules::IDENTITY,
            "agent.fleet_identity",
            json!({}),
        )
        .await;
        let mapped = reply.and_then(|v| map::agent_identities(&v));
        sh.update(|s| match mapped {
            Ok(a) => {
                s.identities.set(a, now());
                sh.identity_ready.notify_one();
            }
            Err(e) => s.identities.fail(e),
        });
    });
    every!(every::AGENTS, |sh, c| {
        let reply = module_call(
            &sh,
            &c,
            modules::RESIDENCE,
            "agent.fleet_residence",
            json!({}),
        )
        .await;
        let mapped = reply.and_then(|v| map::agent_residences(&v));
        sh.update(|s| match mapped {
            Ok(r) => s.residences.set(r, now()),
            Err(e) => s.residences.fail(e),
        });
    });
    // Where agents live: as soon as identity rows exist, then every minute.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        tasks.push(tokio::spawn(async move {
            loop {
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
                tokio::time::sleep(secs(every::PLACES)).await;
            }
        }));
    }
    every!(every::COUNTS, |sh, c| {
        let reply = module_call(&sh, &c, modules::IDENTITY, "enumerate", json!({})).await;
        let mapped = reply.and_then(|v| map::fleet_counts(&v));
        sh.update(|s| match mapped {
            Ok(n) => s.counts.set(n, now()),
            Err(e) => s.counts.fail(e),
        });
    });

    // Health: every five seconds, every second while any action is in
    // flight, so a restart's first healthy check shows promptly.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        tasks.push(tokio::spawn(async move {
            loop {
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
        tasks.push(tokio::spawn(async move {
            loop {
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
        tasks.push(tokio::spawn(async move {
            let mut open: Option<(String, Box<dyn super::ModuleLogFollow>)> = None;
            loop {
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
        tasks.push(tokio::spawn(async move {
            loop {
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
        tasks.push(tokio::spawn(async move {
            loop {
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
    // Each module's retained exits, for the starts and exits list.
    {
        let sh = Arc::clone(shared);
        let c = conn.clone();
        tasks.push(tokio::spawn(async move {
            loop {
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
                tokio::time::sleep(secs(every::TERMINALS)).await;
            }
        }));
    }
    tasks.push(tokio::spawn(spawn_feed(Arc::clone(shared), conn.clone())));
    tasks.push(tokio::spawn(activity_feed(
        Arc::clone(shared),
        conn.clone(),
    )));
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
    if shared.read(|s| s.in_roster(module)) == Some(false) {
        return Err(Fault::sentence(format!("{module} isn't installed")));
    }
    let body = serde_json::to_vec(&json!({ "method": method, "params": params }))
        .map_err(|e| Fault::sentence(e.to_string()))?;
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
                        .map_err(|e| module_error(module, &e))?;
                    routes.insert(module.to_string(), h);
                    h
                }
            }
        };
        match conn
            .consumer
            .request(&handle, body.clone(), call_options())
            .await
        {
            Ok(bytes) => {
                shared.raw(
                    &format!("{module} {method}"),
                    String::from_utf8_lossy(&bytes).into_owned(),
                );
                return serde_json::from_slice(&bytes)
                    .map_err(|_| Fault::sentence(format!("{module} sent an unreadable reply")));
            }
            Err(e) => {
                shared.raw(&format!("{module} {method}"), format!("error: {e}"));
                if route_gone(&e) {
                    conn.routes.lock().await.remove(module);
                    if attempt == 0 {
                        continue;
                    }
                }
                return Err(module_error(module, &e));
            }
        }
    }
    Err(Fault::sentence(format!("{module} unreachable · retrying")))
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

/// Live processes: seeded from the spawn snapshot, then followed. After a
/// lag the feed resubscribes from the last cursor it saw; a cursor the
/// daemon no longer accepts reseeds from a fresh snapshot.
async fn spawn_feed(shared: Arc<Shared>, conn: Conn) {
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
                    shared.update(|s| s.spawn_feed.fail(e.to_string()));
                    tokio::time::sleep(secs(2.0)).await;
                    break;
                }
            }
        }
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

/// The activity ledger: the newest hundred entries, then followed live from
/// the query's head. After a lag it resubscribes from the last entry it
/// has; a refused cursor reseeds with a new query.
async fn activity_feed(shared: Arc<Shared>, conn: Conn) {
    let mut since: Option<ActivityCursor> = None;
    loop {
        if since.is_none() {
            let query = ActivityQueryRequest {
                agent: None,
                project: None,
                module: None,
                kind: None,
                since_ms: None,
                until_ms: None,
                limit: 100,
                cursor: None,
            };
            match conn.consumer.query_activity(query).await {
                Ok(page) => {
                    shared.update(|s| s.activity.set(page.entries, now()));
                    since = Some(page.head);
                }
                Err(e) => {
                    shared.update(|s| s.activity.fail(control_fault(&e)));
                    tokio::time::sleep(secs(5.0)).await;
                    continue;
                }
            }
        }
        let mut sub = match conn.consumer.subscribe_activity(since.clone()).await {
            Ok(sub) => sub,
            Err(e) => {
                shared.update(|s| s.activity.fail(control_fault(&e)));
                tokio::time::sleep(secs(5.0)).await;
                since = None;
                continue;
            }
        };
        loop {
            match sub.next().await {
                Ok(Some(entry)) => {
                    since = Some(ActivityCursor {
                        daemon_incarnation: entry.daemon_incarnation.clone(),
                        seq: entry.seq,
                    });
                    shared.update(|s| {
                        let list = s.activity.value.get_or_insert_with(Vec::new);
                        if !list.iter().any(|e| e.seq == entry.seq) {
                            list.insert(0, entry);
                            list.truncate(200);
                        }
                        s.activity.at = now();
                        s.activity.error = None;
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    let lagged = e.code().is_some_and(|c| c.contains("lag"));
                    if !lagged {
                        shared.update(|s| s.activity.fail(e.to_string()));
                        since = None;
                        tokio::time::sleep(secs(2.0)).await;
                    }
                    break;
                }
            }
        }
    }
}

// ---- host ------------------------------------------------------------------------

/// The machine's memory, load and data-home volume, read locally every ten
/// seconds whether or not the daemon is up.
async fn host_facts(shared: Arc<Shared>) {
    let service = Arc::clone(&shared.service);
    let mut tick = tokio::time::interval(secs(every::HOST));
    loop {
        tick.tick().await;
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
    while let Some(req) = rx.recv().await {
        match req {
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
