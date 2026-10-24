//! The live data layer: one daemon connection, a task per source, and the
//! snapshot the app draws.
//!
//! [`start`] connects through the same discovery `ck` uses (an explicit
//! connection file, then `SUBC_CONNECTION_FILE`, then the default
//! locations) and runs every source as its own task at its own refresh
//! rate. Each source writes its latest reply (or its error) into [`State`];
//! after every write the whole [`Snapshot`] is rebuilt by [`assemble`], a
//! pure function, and sent on the watch channel the app reads. The app never
//! waits on the network.
//!
//! When the connection fails or drops, the snapshot switches to the
//! daemon-down screen and the layer tries again every two seconds.

pub mod activity;
pub mod apply;
pub mod map;
pub mod pairing;
mod run;
pub mod schedule;
pub mod settings;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use subc_client_rs::{ServerDescription, SupervisorProvenanceResponse};
use subc_control::{
    SupervisorEntry, SupervisorHealthEntry, SupervisorHealthStatus, SupervisorRouteModule,
};
use tokio::sync::{mpsc, watch};

use crate::app::Request;
use crate::model::{
    ActionKind, ActionPhase, ActionResult, ActivityEntry, AgentSources, Agents, Approvals, Backups,
    BuildServer, Daemon, DaemonDown, DaemonRunning, Devices, Fault, FaultKind, Host, LogLine,
    LogView, Module, ModuleAction, PaletteCommand, Panel, ProcessEvent, ProcessEventKind, Provider,
    Snapshot, Status, Usage,
};
use crate::time::{Timestamp, UtcOffset};

use map::{AgentIdentity, AgentResidence, CpuTracker};
use schedule::Source;

/// The modules whose replies feed panels, by the id the daemon knows them
/// by.
pub mod modules {
    pub const USAGE: &str = crate::quota::USAGE_MODULE;
    pub const DEVICES: &str = "callosum";
    pub const IDENTITY: &str = "entorhinal";
    pub const RESIDENCE: &str = "prefrontal-core";
    pub const BUILD_SERVER: &str = "motor";
    pub const BACKUPS: &str = "engram";
    pub const MODELS: &str = "fusiform";
}

/// Refresh intervals, in seconds. A panel past twice its interval shows its
/// age; past five times it reads as a last good reading.
pub mod every {
    pub const ROSTER: f64 = 2.0;
    pub const HEALTH: f64 = 5.0;
    pub const DESCRIBE: f64 = 10.0;
    pub const ROUTES: f64 = 10.0;
    pub const HOST: f64 = 10.0;
    pub const USAGE: f64 = 30.0;
    pub const DEVICES: f64 = 15.0;
    pub const AGENTS: f64 = 15.0;
    pub const PLACES: f64 = 60.0;
    pub const COUNTS: f64 = 300.0;
    pub const LOG: f64 = 2.0;
    pub const PROVENANCE: f64 = 60.0;
    pub const TERMINALS: f64 = 30.0;
    /// What a subscription-fed panel (the activity ledger, the spawn feed)
    /// ages against once its subscription has closed. While it is open it
    /// never ages, however quiet.
    pub const CLOSED_FEED: f64 = 15.0;
    /// How often a down daemon is looked for again.
    pub const RECONNECT: f64 = 2.0;
}

/// What only the program embedding the app can answer about the daemon's
/// installed service: the setup engine's view of it, how to start it, and
/// where the daemon's own log is. Methods may block (they run service
/// manager commands); the live layer calls them off the async runtime.
pub trait DaemonService: Send + Sync + 'static {
    /// The service manager's view, such as "loaded · restarts on failure".
    fn service_status(&self) -> String;
    /// Starts the installed service. `Ok` carries what happened, `Err` why
    /// it couldn't.
    fn start_service(&self) -> Result<String, String>;
    /// The last `n` lines of the daemon's own log, oldest first.
    fn daemon_log_tail(&self, n: usize) -> Vec<String>;
    /// The CortexKit data home, whose volume Home's disk gauge shows.
    fn data_home(&self) -> Option<PathBuf>;
    /// Opens the module's log the way `ck module logs <id>` reads it (its
    /// own log files, the daemon's capture of its stderr, and the daemon's
    /// lines naming it): the newest `max` lines, and a follower for what is
    /// appended after them. `None` when this program has no file-log reader,
    /// and the app falls back to the stderr the daemon keeps in memory; a
    /// failed read is `Some(Err)` and shows as the log's error.
    fn open_module_log(&self, module_id: &str, max: usize) -> Option<Result<OpenedLog, String>> {
        let _ = (module_id, max);
        None
    }
}

/// A module's log as first opened: its newest lines, oldest first, and how
/// to follow it from there.
pub struct OpenedLog {
    pub lines: Vec<ModuleLogLine>,
    pub follow: Box<dyn ModuleLogFollow>,
}

/// Follows an open module log: each poll returns only the lines appended
/// since the one before, never a re-read of what was already returned.
pub trait ModuleLogFollow: Send {
    fn poll(&mut self) -> Result<Vec<ModuleLogLine>, String>;
}

/// One line of a module's merged log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleLogLine {
    /// When the line was written (or, for an unstamped capture line, the
    /// nearest stamp before it), in Unix milliseconds.
    pub at_ms: Option<i64>,
    /// The level the line states, if it states one.
    pub level: Option<crate::model::LogLevel>,
    /// The whole line as written.
    pub text: String,
}

/// A [`DaemonService`] that knows nothing: service "unknown", no start, no
/// log.
pub struct NoService;

impl DaemonService for NoService {
    fn service_status(&self) -> String {
        "unknown".into()
    }
    fn start_service(&self) -> Result<String, String> {
        Err("this program can't start the daemon".into())
    }
    fn daemon_log_tail(&self, _n: usize) -> Vec<String> {
        Vec::new()
    }
    fn data_home(&self) -> Option<PathBuf> {
        None
    }
}

pub struct LiveOptions {
    /// An explicit connection file (`ck --subc`); otherwise discovery.
    pub connection_file: Option<PathBuf>,
    pub service: Arc<dyn DaemonService>,
    /// How the top bar names this machine.
    pub host_label: String,
    /// The host's offset from UTC, for clock times.
    pub utc_offset: UtcOffset,
    /// The home folder, for showing paths under it as `~/…`.
    pub home: Option<String>,
}

impl LiveOptions {
    pub fn new(connection_file: Option<PathBuf>, service: Arc<dyn DaemonService>) -> LiveOptions {
        LiveOptions {
            connection_file,
            service,
            host_label: "This computer".into(),
            utc_offset: local_utc_offset(),
            home: home_folder(),
        }
    }
}

/// The home folder from the environment: `USERPROFILE` on Windows, `HOME`
/// elsewhere; `None` when unset or empty.
pub fn home_folder() -> Option<String> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(var).ok().filter(|h| !h.is_empty())
}

/// The local offset from UTC right now.
pub fn local_utc_offset() -> UtcOffset {
    use chrono::Offset;
    UtcOffset(chrono::Local::now().offset().fix().local_minus_utc())
}

/// A running live layer: the snapshots it produces and where the app's
/// requests go.
pub struct Live {
    pub snapshots: watch::Receiver<Snapshot>,
    pub requests: mpsc::UnboundedSender<Request>,
    /// The latest reply (JSON, or debug text for replies with no JSON form)
    /// or error recorded for each daemon and module request, by name
    /// ("supervisor.list", "insula usage.get", …), for diagnosing a reply
    /// the mapping doesn't understand.
    pub raw: Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
}

/// Starts the live layer on the current tokio runtime.
pub fn start(options: LiveOptions) -> Live {
    let state = State::new(&options);
    let (tx, rx) = watch::channel(assemble(&state, Timestamp::system_now()));
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let raw = Arc::default();
    run::spawn(options, state, tx, req_rx, Arc::clone(&raw));
    Live {
        snapshots: rx,
        requests: req_tx,
        raw,
    }
}

/// Whether every source has reported once (data or an error), or the
/// daemon is down. The text frame waits for this before printing.
pub fn all_reported(s: &Snapshot) -> bool {
    fn done<T>(p: &Panel<T>) -> bool {
        p.loaded || p.error.is_some()
    }
    if matches!(s.daemon.data, Daemon::Down(_)) && s.daemon.loaded {
        return true;
    }
    done(&s.host)
        && done(&s.daemon)
        && done(&s.modules)
        && done(&s.build_server)
        && done(&s.backups)
        && done(&s.usage)
        && done(&s.activity)
        && done(&s.agents)
        && done(&s.devices)
}

// ---- state ---------------------------------------------------------------------

/// One source's latest reading, when it arrived, and its latest error.
#[derive(Clone, Debug)]
pub struct Part<T> {
    pub value: Option<T>,
    pub at: Timestamp,
    pub error: Option<Fault>,
}

impl<T> Default for Part<T> {
    fn default() -> Self {
        Part {
            value: None,
            at: Timestamp::from_unix_millis(0),
            error: None,
        }
    }
}

impl<T> Part<T> {
    pub fn set(&mut self, value: T, at: Timestamp) {
        self.value = Some(value);
        self.at = at;
        self.error = None;
    }

    /// Records a failure. The last good value stays, to be shown as such.
    pub fn fail(&mut self, error: impl Into<Fault>) {
        self.error = Some(error.into());
    }

    fn panel<U>(&self, refresh: Option<f64>, f: impl FnOnce(&T) -> U, placeholder: U) -> Panel<U> {
        match &self.value {
            Some(v) => Panel {
                received_at: self.at,
                data: f(v),
                refresh_secs: refresh,
                error: self.error.as_ref().map(|f| f.text.clone()),
                error_kind: self.error.as_ref().map(|f| f.kind).unwrap_or_default(),
                loaded: true,
            },
            None => Panel {
                error: self.error.as_ref().map(|f| f.text.clone()),
                error_kind: self.error.as_ref().map(|f| f.kind).unwrap_or_default(),
                ..Panel::waiting(placeholder, refresh)
            },
        }
    }
}

/// The host's own facts, read locally.
#[derive(Clone, Debug, Default)]
pub struct HostFacts {
    pub memory: Option<subc_os::host::MemoryInfo>,
    pub load: Option<[f64; 3]>,
    pub disk: Option<subc_os::host::DiskSpace>,
}

/// One direct health probe: when it was taken, and the module's detail and
/// complete metrics.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    pub at: Timestamp,
    pub detail: Option<String>,
    pub metrics: Option<serde_json::Value>,
}

/// How long after its process starts a running module without a health
/// check yet reads as starting rather than of unknown health.
pub const START_GRACE_SECS: f64 = 120.0;

/// How often a module whose cached metrics are over the cache's size cap is
/// probed directly for the full set, at most.
pub const TRUNCATED_PROBE_SECS: f64 = 30.0;

/// The connection as the live layer last saw it.
#[derive(Clone, Debug)]
pub enum Connection {
    /// The first attempt hasn't finished.
    Connecting,
    Up {
        daemon_ver: String,
    },
    Down {
        /// When ck lost a connection it had; `None` if it never had one.
        lost_at: Option<Timestamp>,
        /// Why the last attempt failed.
        reason: String,
    },
}

/// The process a module runs now, from the spawn feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveProcess {
    pub pid: u32,
    pub generation: u64,
    pub spawned_at: Timestamp,
}

/// Everything the sources have said, from which [`assemble`] builds a
/// snapshot.
#[derive(Clone, Debug)]
pub struct State {
    pub host_label: String,
    pub utc_offset: UtcOffset,
    pub home: Option<String>,
    pub connection: Connection,
    pub connected_at: Part<()>,
    pub roster: Part<Vec<SupervisorEntry>>,
    pub health: Part<Vec<SupervisorHealthEntry>>,
    pub provenance: Part<SupervisorProvenanceResponse>,
    pub describe: Part<ServerDescription>,
    pub settings: settings::SettingsReading,
    pub routes: Part<Vec<SupervisorRouteModule>>,
    /// Live processes by module, from the spawn snapshot and events.
    pub processes: HashMap<String, LiveProcess>,
    /// When each module's process last exited, as ck saw it.
    pub exits: HashMap<String, Timestamp>,
    pub spawn_feed: Part<()>,
    /// Whether the spawn subscription is open now.
    pub spawn_open: bool,
    /// Starts and exits, newest first.
    pub process_events: Vec<ProcessEvent>,
    pub cpu: CpuTracker,
    /// ck's own build-server load samples, for motor builds that send no
    /// history.
    pub build_samples: Vec<f64>,
    /// When the last of `build_samples` was taken.
    pub build_sample_at: Option<Timestamp>,
    pub usage: Part<Vec<Provider>>,
    pub devices: Part<Devices>,
    pub identities: Part<Vec<AgentIdentity>>,
    pub residences: Part<HashMap<String, AgentResidence>>,
    /// The token the last full `agent.fleet_identity` reply gave, sent with
    /// the next poll so an unchanged fleet answers without its rows.
    pub identity_token: Option<serde_json::Value>,
    /// The same for `agent.fleet_residence`.
    pub residence_token: Option<serde_json::Value>,
    pub places: Part<HashMap<String, String>>,
    pub counts: Part<(Option<u32>, Option<u32>)>,
    /// When the activity ledger was last heard from (a page, an entry, or
    /// its subscription opening), and its latest error.
    pub activity: Part<()>,
    /// The activity ledger as loaded, and where its feed resumes.
    pub ledger: activity::ActivityLedger,
    pub host: Part<HostFacts>,
    /// The module whose log is on screen.
    pub focus: Option<String>,
    pub log: Part<(String, Vec<LogLine>)>,
    /// Where the log's lines come from, for the log view's footer, with
    /// `{module}` standing for the module's id.
    pub log_source: String,
    /// The latest direct health probe of each module that has had one: the
    /// selected module's, and any whose cached metrics are only the cache's
    /// size marker.
    pub probes: HashMap<String, Probe>,
    /// Each module's recent process exits, as the daemon's terminal journal
    /// retains them. They become the exit rows in the "Starts and exits"
    /// list on the Modules screen.
    pub terminals: HashMap<String, Vec<ProcessEvent>>,
    pub actions: HashMap<String, ModuleAction>,
    pub last_result: Option<ActionResult>,
    pub pending_restarts: std::collections::HashSet<String>,
    /// The status each module had when ck last looked, and since when.
    pub status_since: HashMap<String, (Status, Option<Timestamp>)>,
    pub service: String,
    pub daemon_log: Vec<String>,
    pub down_note: Option<String>,
    /// The latest results of the Add a phone exchange, numbered in arrival
    /// order (see [`State::record_pairing`]).
    pub pairing: std::collections::VecDeque<(u64, crate::pairing::flow::Event)>,
    /// The number the next pairing result gets.
    pub pairing_seq: u64,
    /// The latest Save preview replies, numbered in arrival order (see
    /// [`State::record_preview`]).
    pub previews: std::collections::VecDeque<(u64, settings::PreviewReply)>,
    /// The number the next preview reply gets.
    pub preview_seq: u64,
    pub applies: std::collections::VecDeque<(u64, settings::ApplyReply)>,
    pub apply_seq: u64,
    /// How many times the live layer has connected to the daemon.
    pub daemon_connection: u64,
}

/// How many pairing results the snapshot keeps. The app applies them as
/// each snapshot arrives, so it falls this far behind only if it stops.
const PAIRING_KEPT: usize = 64;

/// How many preview replies the snapshot keeps. Only the newest request's
/// reply matters to the app, and it applies replies as snapshots arrive.
const PREVIEWS_KEPT: usize = 64;

impl State {
    fn new(options: &LiveOptions) -> State {
        State {
            host_label: options.host_label.clone(),
            utc_offset: options.utc_offset,
            home: options.home.clone(),
            connection: Connection::Connecting,
            connected_at: Part::default(),
            roster: Part::default(),
            health: Part::default(),
            provenance: Part::default(),
            describe: Part::default(),
            settings: settings::SettingsReading::default(),
            routes: Part::default(),
            processes: HashMap::new(),
            exits: HashMap::new(),
            spawn_feed: Part::default(),
            spawn_open: false,
            process_events: Vec::new(),
            cpu: CpuTracker::default(),
            build_samples: Vec::new(),
            build_sample_at: None,
            usage: Part::default(),
            devices: Part::default(),
            identities: Part::default(),
            residences: Part::default(),
            identity_token: None,
            residence_token: None,
            places: Part::default(),
            counts: Part::default(),
            activity: Part::default(),
            ledger: activity::ActivityLedger::default(),
            host: Part::default(),
            focus: None,
            log: Part::default(),
            log_source: String::new(),
            probes: HashMap::new(),
            terminals: HashMap::new(),
            actions: HashMap::new(),
            last_result: None,
            pending_restarts: std::collections::HashSet::new(),
            status_since: HashMap::new(),
            service: "unknown".into(),
            daemon_log: Vec::new(),
            down_note: None,
            pairing: std::collections::VecDeque::new(),
            pairing_seq: 1,
            previews: std::collections::VecDeque::new(),
            preview_seq: 1,
            applies: std::collections::VecDeque::new(),
            apply_seq: 1,
            daemon_connection: 0,
        }
    }

    /// Queues a preview reply for the app, under the next number.
    pub fn record_preview(&mut self, reply: settings::PreviewReply) {
        self.previews.push_back((self.preview_seq, reply));
        self.preview_seq += 1;
        while self.previews.len() > PREVIEWS_KEPT {
            self.previews.pop_front();
        }
    }

    pub fn record_apply(&mut self, reply: settings::ApplyReply) {
        self.applies.push_back((self.apply_seq, reply));
        self.apply_seq += 1;
        while self.applies.len() > PREVIEWS_KEPT {
            self.applies.pop_front();
        }
    }

    /// Queues a pairing result for the app, under the next number.
    pub fn record_pairing(&mut self, event: crate::pairing::flow::Event) {
        self.pairing.push_back((self.pairing_seq, event));
        self.pairing_seq += 1;
        while self.pairing.len() > PAIRING_KEPT {
            self.pairing.pop_front();
        }
    }

    /// For tests: a state with nothing read yet.
    pub fn empty(host_label: &str) -> State {
        State::new(&LiveOptions {
            connection_file: None,
            service: Arc::new(NoService),
            host_label: host_label.into(),
            utc_offset: UtcOffset::UTC,
            home: None,
        })
    }

    pub fn health_of(&self, module: &str) -> Option<&SupervisorHealthEntry> {
        self.health
            .value
            .as_ref()?
            .iter()
            .find(|h| h.module_id == module)
    }

    /// A module's metrics as the app should read them: the cached ones,
    /// unless the cache holds only its size marker (or the module is
    /// selected), when the latest direct probe's are used.
    pub fn metrics_of(&self, module: &str) -> Option<&serde_json::Value> {
        let cached = self.health_of(module).and_then(|h| h.metrics.as_ref());
        let probe = self.probes.get(module);
        let prefer_probe =
            self.focus.as_deref() == Some(module) || cached.is_some_and(map::is_truncation_marker);
        match (prefer_probe, probe) {
            (true, Some(p)) => p.metrics.as_ref(),
            _ => cached,
        }
    }

    /// A module's health detail, from the same source as [`State::metrics_of`].
    pub fn detail_of(&self, module: &str) -> Option<&str> {
        let cached = self.health_of(module);
        let probe = self.probes.get(module);
        let prefer_probe = self.focus.as_deref() == Some(module)
            || cached
                .and_then(|h| h.metrics.as_ref())
                .is_some_and(map::is_truncation_marker);
        match (prefer_probe, probe) {
            (true, Some(p)) => p.detail.as_deref(),
            _ => cached.and_then(|h| h.detail.as_deref()),
        }
    }

    /// Modules whose cached health metrics were replaced by the cache's size
    /// marker, and that are due a direct health probe: either they have never
    /// had one, or the last one is at least [`TRUNCATED_PROBE_SECS`] old.
    pub fn truncated_due(&self, now: Timestamp) -> Vec<String> {
        self.health
            .value
            .iter()
            .flatten()
            .filter(|h| h.metrics.as_ref().is_some_and(map::is_truncation_marker))
            .filter(|h| {
                self.probes
                    .get(&h.module_id)
                    .is_none_or(|p| now.secs_since(p.at) >= TRUNCATED_PROBE_SECS)
            })
            .map(|h| h.module_id.clone())
            .collect()
    }

    /// The params for the next poll of a module source in
    /// [`schedule::POLLED`]: the fleet polls carry the last token their
    /// source gave; every other source takes none.
    pub fn poll_params(&self, source: Source) -> serde_json::Value {
        let token = match source {
            Source::Identity => self.identity_token.as_ref(),
            Source::Residence => self.residence_token.as_ref(),
            _ => None,
        };
        match token {
            Some(t) => serde_json::json!({ "token": t }),
            None => serde_json::json!({}),
        }
    }

    /// Records a reply (or failure) from one of the module sources in
    /// [`schedule::POLLED`], read at `at`. Returns whether it was a good
    /// reading. A failure keeps the source's last good reading.
    pub fn record_module_reply(
        &mut self,
        source: Source,
        reply: Result<serde_json::Value, Fault>,
        at: Timestamp,
    ) -> bool {
        fn put<T>(part: &mut Part<T>, mapped: Result<T, Fault>, at: Timestamp) -> bool {
            match mapped {
                Ok(v) => {
                    part.set(v, at);
                    true
                }
                Err(e) => {
                    part.fail(e);
                    false
                }
            }
        }
        // A fleet poll sent with a token is answered `unchanged: true`, with
        // no rows, while nothing changed: the rows on hand are then current
        // as of this reply, so they stay and their age starts again. A full
        // reply replaces them and gives the token for the next poll.
        fn fleet<T>(
            part: &mut Part<T>,
            token: &mut Option<serde_json::Value>,
            reply: Result<serde_json::Value, Fault>,
            rows: impl FnOnce(&serde_json::Value) -> Result<T, Fault>,
            at: Timestamp,
        ) -> bool {
            let reply = match reply {
                Ok(v) => v,
                Err(e) => {
                    part.fail(e);
                    return false;
                }
            };
            if let Some(e) = map::reply_error(&reply) {
                part.fail(e);
                return false;
            }
            let (unchanged, next) = map::fleet_change(&reply);
            if !unchanged {
                let good = put(part, rows(&reply), at);
                if good {
                    *token = next;
                }
                return good;
            }
            if part.value.is_none() {
                // Nothing to keep: forget the token, so the next poll asks
                // for every row.
                *token = None;
                return false;
            }
            part.at = at;
            part.error = None;
            if next.is_some() {
                *token = next;
            }
            true
        }
        match source {
            Source::Usage => put(&mut self.usage, reply.and_then(|v| map::usage(&v, at)), at),
            Source::Devices => put(
                &mut self.devices,
                reply.and_then(|v| map::devices(&v, at)),
                at,
            ),
            Source::Identity => fleet(
                &mut self.identities,
                &mut self.identity_token,
                reply,
                map::agent_identities,
                at,
            ),
            Source::Residence => fleet(
                &mut self.residences,
                &mut self.residence_token,
                reply,
                map::agent_residences,
                at,
            ),
            Source::Counts => put(
                &mut self.counts,
                reply.and_then(|v| map::fleet_counts(&v)),
                at,
            ),
            _ => false,
        }
    }

    pub fn in_roster(&self, module: &str) -> Option<bool> {
        self.roster
            .value
            .as_ref()
            .map(|r| r.iter().any(|e| e.module_id == module))
    }

    /// Records a new roster: the CPU and memory readings it carries, and
    /// when each module's status changed.
    pub fn record_roster(&mut self, roster: Vec<SupervisorEntry>, at: Timestamp) {
        self.record_cpu(&roster, at);
        self.roster.set(roster, at);
        self.note_statuses(at);
    }

    /// Records the CPU and memory readings a roster carries, without
    /// publishing the roster. A CPU percent needs two readings of the same
    /// process, so the first roster at startup goes only here.
    pub fn record_cpu(&mut self, roster: &[SupervisorEntry], at: Timestamp) {
        for e in roster {
            let pid = self.processes.get(&e.module_id).map(|p| p.pid);
            match map::resource_reading(e) {
                Some((cpu_ms, mem)) if e.state != "stopped" && e.state != "disabled" => self
                    .cpu
                    .record(&e.module_id, pid, at.unix_millis(), cpu_ms, mem),
                _ => {
                    if pid.is_none() {
                        self.cpu.forget(&e.module_id);
                    }
                }
            }
        }
    }

    fn note_statuses(&mut self, at: Timestamp) {
        let ids: Vec<(String, Status)> = self
            .roster
            .value
            .iter()
            .flatten()
            .map(|e| (e.module_id.clone(), self.status(e)))
            .collect();
        for (id, status) in ids {
            match self.status_since.get(&id) {
                Some((s, _)) if *s == status => {}
                // A change ck saw happen: it started now. The first look
                // can't say when the status began.
                Some(_) => {
                    self.status_since.insert(id, (status, Some(at)));
                }
                None => {
                    self.status_since.insert(id, (status, None));
                }
            }
        }
    }

    fn status(&self, e: &SupervisorEntry) -> Status {
        let health = self
            .health_of(&e.module_id)
            .map(|h| h.status)
            .unwrap_or(e.health);
        match map::module_status(&e.state, health) {
            Status::Unknown if self.not_yet_checked(e) => Status::Starting,
            status => status,
        }
    }

    /// A running module whose current process has had no health check yet
    /// and started within [`START_GRACE_SECS`]: it is starting, not of
    /// unknown health. Past that grace a module nobody can check stays
    /// unknown.
    fn not_yet_checked(&self, e: &SupervisorEntry) -> bool {
        if e.state != "running" {
            return false;
        }
        let Some(process) = self.processes.get(&e.module_id) else {
            return false;
        };
        let probed = self
            .health_of(&e.module_id)
            .and_then(|h| h.last_probe_ms)
            .or(e.last_probe_ms)
            .map(|ms| Timestamp::from_unix_millis(ms as i64));
        let checked_since_start = probed.is_some_and(|p| p >= process.spawned_at);
        !checked_since_start && self.roster.at.secs_since(process.spawned_at) < START_GRACE_SECS
    }

    /// A one-word lifecycle label ("Restarting", "Starting" and so on) taken
    /// from the supervisor's state, for a module that isn't simply running.
    /// A running module whose current process hasn't had a health check yet,
    /// and started under [`START_GRACE_SECS`] ago, is labelled "Starting". A
    /// running module past that point gets no label (`None`).
    fn lifecycle(&self, e: &SupervisorEntry) -> Option<String> {
        let word = match e.state.as_str() {
            "restarting" => "Restarting",
            "starting" => "Starting",
            "draining" => "Draining",
            "stopped" => "Stopped",
            "disabled" => "Disabled",
            "failed" => "Failed",
            "unresponsive" => "Not responding",
            "running" if self.not_yet_checked(e) => "Starting",
            _ => return None,
        };
        Some(word.to_string())
    }
}

// ---- assembly ------------------------------------------------------------------

/// The snapshot the sources' latest readings describe, as of `now`.
pub fn assemble(state: &State, now: Timestamp) -> Snapshot {
    let host = state.host.panel(
        Some(every::HOST),
        |h| host_view(state, h),
        host_view(state, &HostFacts::default()),
    );
    let daemon = daemon_panel(state, now);
    let modules = state.roster.panel(
        Some(every::ROSTER),
        |r| r.iter().map(|e| module_view(state, e)).collect(),
        Vec::new(),
    );
    let commands = palette_commands(&modules.data);

    let health_metrics = |id: &str| state.metrics_of(id);
    let build_server = module_backed(state, modules::BUILD_SERVER, Some(every::HEALTH), || {
        map::build_server(health_metrics(modules::BUILD_SERVER), &state.build_samples)
    });
    let backups = module_backed(state, modules::BACKUPS, Some(every::HEALTH), || {
        map::backups(
            health_metrics(modules::BACKUPS),
            state.detail_of(modules::BACKUPS),
        )
    });
    let (models_priced, models_checked_at) = map::models(
        health_metrics(modules::MODELS),
        state
            .health_of(modules::MODELS)
            .and_then(|h| h.last_probe_ms)
            .map(|ms| Timestamp::from_unix_millis(ms as i64)),
    );
    let models_counted = map::model_count(health_metrics(modules::MODELS));
    let mut usage = state.usage.panel(
        Some(every::USAGE),
        |p| Usage {
            providers: p.clone(),
            models_priced,
            models_counted,
            models_checked_at,
        },
        Usage {
            providers: Vec::new(),
            models_priced,
            models_counted,
            models_checked_at,
        },
    );
    if state.in_roster(modules::USAGE) == Some(false) {
        usage.error = Some(format!("{} isn't installed", modules::USAGE));
        usage.error_kind = FaultKind::Sentence;
    }

    let names = state
        .identities
        .value
        .as_deref()
        .map(map::agent_names)
        .unwrap_or_default();
    let activity = feed_panel(
        &state.activity,
        state.ledger.open,
        state.home_activity(&names),
        Vec::new(),
    );
    let activity_log = feed_panel(
        &state.activity,
        state.ledger.open,
        state.activity_log(&names),
        crate::model::ActivityLog::default(),
    );
    let terminal_exits: Vec<ProcessEvent> = state.terminals.values().flatten().cloned().collect();
    let process_events = {
        let events = starts_and_exits(&state.process_events, &activity.data, &terminal_exits);
        if state.spawn_feed.value.is_some() {
            feed_panel(&state.spawn_feed, state.spawn_open, events, Vec::new())
        } else {
            feed_panel(&state.activity, state.ledger.open, events, Vec::new())
        }
    };

    let mut agents = state.identities.panel(
        Some(every::AGENTS),
        |ids| {
            map::agents(
                ids,
                state.residences.value.as_ref(),
                state.places.value.as_ref(),
                state.counts.value.unwrap_or((None, None)),
                now,
            )
        },
        Agents {
            list: Vec::new(),
            working: 0,
            machines: None,
            workspaces: None,
            projects: None,
            workspace_list: Vec::new(),
            machine_list: Vec::new(),
            sources: AgentSources::default(),
        },
    );
    // Each column of the Agents screen ages and dims by the source it comes
    // from.
    agents.data.sources = AgentSources {
        identity: state.identities.panel(Some(every::AGENTS), |_| (), ()),
        state: state.residences.panel(Some(every::AGENTS), |_| (), ()),
        places: state.places.panel(Some(every::PLACES), |_| (), ()),
        counts: state.counts.panel(Some(every::COUNTS), |_| (), ()),
    };
    // Without state the agents still list, each "unknown"; the refusal (or
    // failure) shows in the panel's heading.
    if agents.error.is_none() {
        if let Some(err) = &state.residences.error {
            agents.error = Some(format!("state {err}"));
        }
    }
    let devices = state
        .devices
        .panel(Some(every::DEVICES), Devices::clone, Devices::default());
    let log_view = state.focus.as_ref().map(|id| {
        let mut p = state.log.panel(
            Some(every::LOG),
            |(m, lines)| LogView {
                module: m.clone(),
                lines: lines.clone(),
                source: state.log_source.replace("{module}", m),
            },
            LogView {
                module: id.clone(),
                lines: Vec::new(),
                source: String::new(),
            },
        );
        if p.data.module != *id {
            p = Panel::waiting(
                LogView {
                    module: id.clone(),
                    lines: Vec::new(),
                    source: String::new(),
                },
                Some(every::LOG),
            );
        }
        p
    });

    Snapshot {
        host,
        daemon,
        modules,
        build_server,
        backups,
        usage,
        activity,
        activity_log,
        process_events,
        agents,
        devices,
        approvals: Panel::new(now, Approvals { waiting: None }),
        commands,
        log_view,
        last_result: state.last_result.clone(),
        serves: crate::model::Serves::LIVE,
        pairing: state.pairing.iter().cloned().collect(),
        previews: state.previews.iter().cloned().collect(),
        applies: state.applies.iter().cloned().collect(),
        daemon_connection: state.daemon_connection,
        settings: state.settings.clone(),
    }
}

/// The palette's commands: the app's own (each tab, the keys page, the
/// theme, quitting), then for each module what the app can do with it:
/// restart, stop or start, its log and its metrics. Nothing it can't do:
/// the palette leaves out the tabs that don't show
/// ([`crate::screens::overlays::shown_commands`]).
pub fn palette_commands(modules: &[Module]) -> Vec<PaletteCommand> {
    let cmd = |title: String, area: &str| PaletteCommand {
        title,
        area: area.into(),
    };
    let tabs = crate::model::Tab::ALL.map(crate::model::Tab::name);
    let mut out: Vec<PaletteCommand> = tabs
        .into_iter()
        .chain(["Keys", "Light or dark", "Quit"])
        .map(|t| cmd(t.to_string(), "App"))
        .collect();
    for m in modules {
        out.push(cmd(format!("Restart {}", m.id), "Modules"));
        let stop_or_start = if m.status == Status::Stopped {
            "Start"
        } else {
            "Stop"
        };
        out.push(cmd(format!("{stop_or_start} {}", m.id), "Modules"));
        out.push(cmd(format!("Open {} logs", m.id), "Modules"));
        out.push(cmd(format!("All metrics for {}", m.id), "Modules"));
    }
    out
}

/// A panel fed by a subscription: "live" while it is open, however quiet,
/// since an open subscription would have said if anything happened. Once
/// closed it keeps its error, if any, and ages from its last message
/// against [`every::CLOSED_FEED`].
fn feed_panel<U>(part: &Part<()>, open: bool, data: U, placeholder: U) -> Panel<U> {
    let refresh = if open { None } else { Some(every::CLOSED_FEED) };
    part.panel(refresh, |_| data, placeholder)
}

/// A panel fed by one module's health: an error while that module isn't
/// installed or health can't be read, else the mapped value.
fn module_backed<T>(
    state: &State,
    module: &str,
    refresh: Option<f64>,
    f: impl FnOnce() -> T,
) -> Panel<T>
where
    T: Placeholder,
{
    if state.in_roster(module) == Some(false) {
        return Panel {
            error: Some(format!("{module} isn't installed")),
            error_kind: FaultKind::Sentence,
            ..Panel::waiting(T::placeholder(), refresh)
        };
    }
    state.health.panel(refresh, |_| f(), T::placeholder())
}

/// The value a panel holds before its first reading.
pub trait Placeholder {
    fn placeholder() -> Self;
}

impl Placeholder for BuildServer {
    fn placeholder() -> Self {
        map::build_server(None, &[])
    }
}

impl Placeholder for Backups {
    fn placeholder() -> Self {
        map::backups(None, None)
    }
}

fn host_view(state: &State, h: &HostFacts) -> Host {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    Host {
        label: state.host_label.clone(),
        machine_id: state
            .describe
            .value
            .as_ref()
            .and_then(|d| d.machine_id.clone()),
        utc_offset: state.utc_offset,
        home: state.home.clone(),
        memory_used_gb: h
            .memory
            .map(|m| m.total_bytes.saturating_sub(m.available_bytes) as f64 / GIB),
        memory_total_gb: h.memory.map(|m| m.total_bytes as f64 / GIB),
        disk_used_gb: h
            .disk
            .map(|d| d.total_bytes.saturating_sub(d.free_bytes) as f64 / 1e9),
        disk_total_gb: h.disk.map(|d| d.total_bytes as f64 / 1e9),
        load: h.load,
    }
}

fn daemon_panel(state: &State, now: Timestamp) -> Panel<Daemon> {
    match &state.connection {
        Connection::Up { daemon_ver } => {
            let started_at = state
                .provenance
                .value
                .as_ref()
                .and_then(|p| p.daemon.daemon_observed.started_at_ms)
                .map(|ms| Timestamp::from_unix_millis(ms as i64));
            let clients = state
                .describe
                .value
                .as_ref()
                .map(|d| d.connected_clients.min(u64::from(u32::MAX)) as u32);
            let mut p = Panel::new(
                state.connected_at.at,
                Daemon::Running(DaemonRunning {
                    version: daemon_ver.clone(),
                    started_at,
                    clients,
                }),
            );
            p.error = state.describe.error.as_ref().map(|f| f.text.clone());
            p.error_kind = state
                .describe
                .error
                .as_ref()
                .map(|f| f.kind)
                .unwrap_or_default();
            if state.describe.value.is_some() {
                p.received_at = state.describe.at;
                p.refresh_secs = Some(every::DESCRIBE);
            }
            p
        }
        Connection::Down { lost_at, .. } => {
            let log_tail: Vec<LogLine> = state
                .daemon_log
                .iter()
                .map(|l| {
                    let (at, level, message) = map::parse_log_line(l);
                    LogLine { at, level, message }
                })
                .collect();
            let last_line = state
                .daemon_log
                .iter()
                .rev()
                .find(|l| !l.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            Panel::new(
                now,
                Daemon::Down(DaemonDown {
                    stopped_at: *lost_at,
                    stopped_by: if lost_at.is_some() {
                        "when ck lost its connection".into()
                    } else {
                        String::new()
                    },
                    last_line,
                    service: state.service.clone(),
                    note: Some(
                        state
                            .down_note
                            .clone()
                            .unwrap_or_else(|| "Checking for the daemon every 2 s.".into()),
                    ),
                    log_tail,
                }),
            )
        }
        Connection::Connecting => Panel::waiting(
            Daemon::Running(DaemonRunning {
                version: "unknown".into(),
                started_at: None,
                clients: None,
            }),
            None,
        ),
    }
}

fn module_view(state: &State, e: &SupervisorEntry) -> Module {
    let id = &e.module_id;
    let status = state.status(e);
    let running = !matches!(e.state.as_str(), "stopped" | "disabled");
    let provenance = state
        .provenance
        .value
        .as_ref()
        .and_then(|p| p.modules.iter().find(|m| &m.module_id == id));
    let process = state.processes.get(id).copied();
    let (pid, started_at) = match process {
        Some(p) => (Some(p.pid), Some(p.spawned_at)),
        None if running => (
            provenance.and_then(|p| p.daemon_observed.pid),
            provenance
                .and_then(|p| p.daemon_observed.spawned_at_ms)
                .map(|ms| Timestamp::from_unix_millis(ms as i64)),
        ),
        None => (None, None),
    };
    let (build, commit) = map::module_build(provenance);
    let (cpu, mem) = if running {
        state.cpu.current(id)
    } else {
        (None, None)
    };
    let (cpu_history, memory_history) = if running {
        state.cpu.history(id)
    } else {
        (Vec::new(), Vec::new())
    };
    let health = state.health_of(id);
    // The selected module's pane, and any module whose cached metrics are
    // over the cache's size cap, read the latest direct probe.
    let (summary, metrics) =
        map::detail_and_metrics(state.detail_of(id), state.metrics_of(id), state.utc_offset);
    let all_metrics = map::every_metric(state.metrics_of(id), state.utc_offset);
    let route_module = state
        .routes
        .value
        .as_ref()
        .map(|r| r.iter().find(|m| &m.module_id == id));
    let serving = route_module.map(|m| m.map(map::route_consumers).unwrap_or_default());
    let log = match (&state.focus, &state.log.value) {
        (Some(f), Some((m, lines))) if f == id && m == id => lines.clone(),
        _ => Vec::new(),
    };
    Module {
        id: id.clone(),
        status,
        status_since: state.status_since.get(id).and_then(|(_, t)| *t),
        pid,
        started_at,
        restarts: e.lifetime_restarts.or(e.restart_count),
        build,
        commit,
        cpu_percent: cpu,
        memory_mb: mem,
        summary,
        routes: serving.as_ref().map(|s| s.len() as u32),
        serving,
        pending_restart: map::pending_restart(e.pending_reload.as_ref()).or_else(|| {
            state
                .pending_restarts
                .contains(&e.module_id)
                .then(|| "settings".into())
        }),
        health_checked_at: health
            .and_then(|h| h.last_probe_ms)
            .or(e.last_probe_ms)
            .map(|ms| Timestamp::from_unix_millis(ms as i64)),
        metrics,
        cpu_history,
        memory_history,
        log,
        action: state.actions.get(id).cloned(),
        drain_limit_secs: e.drain_timeout_ms.map(|ms| ms as f64 / 1000.0),
        lifecycle: state.lifecycle(e),
        all_metrics,
    }
}

// ---- agents' places ------------------------------------------------------------

/// The most agent ids one `agent.where_many` request may carry.
pub const WHERE_MANY_BATCH: usize = 64;

/// One round of the agents-place source: nothing (`None`) until there are
/// identity rows; then one `agent.where_many` call through `call` per batch
/// of up to [`WHERE_MANY_BATCH`] ids, merged. The first failing batch's error
/// is the round's.
pub async fn where_many_round<F, Fut>(
    ids: &[String],
    here: &str,
    mut call: F,
) -> Option<Result<HashMap<String, String>, Fault>>
where
    F: FnMut(serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, Fault>>,
{
    if ids.is_empty() {
        return None;
    }
    let mut places = HashMap::new();
    for batch in ids.chunks(WHERE_MANY_BATCH) {
        let reply = match call(serde_json::json!({ "agentIds": batch })).await {
            Ok(reply) => reply,
            Err(e) => return Some(Err(e)),
        };
        match map::agent_places(&reply, here) {
            Ok(p) => places.extend(p),
            Err(e) => return Some(Err(e)),
        }
    }
    Some(Ok(places))
}

// ---- starts and exits ----------------------------------------------------------

/// Starts and exits for the Modules screen (which shows today's), newest
/// first: the spawn
/// feed's events (which carry pids) since ck connected, module starts and
/// restarts from the activity ledger, and exits the daemon retained. The same
/// start seen in the spawn feed and the ledger, or the same exit in the feed
/// and the retained exits, is listed once.
pub fn starts_and_exits(
    spawn: &[ProcessEvent],
    activity: &[ActivityEntry],
    exits: &[ProcessEvent],
) -> Vec<ProcessEvent> {
    let near = map::same_event;
    let ledger_starts: Vec<ProcessEvent> = activity
        .iter()
        .filter(|a| {
            a.kind == "module" && (a.what.starts_with("restarted") || a.what.starts_with("started"))
        })
        .map(|a| ProcessEvent {
            at: a.at,
            module: a.subject.clone(),
            what: a.what.clone(),
            kind: ProcessEventKind::Started,
        })
        .collect();
    let mut out: Vec<ProcessEvent> = Vec::new();
    for e in spawn {
        let covered = e.kind == ProcessEventKind::Exited && exits.iter().any(|x| near(x, e));
        if !covered {
            out.push(e.clone());
        }
    }
    for e in ledger_starts.iter().chain(exits) {
        if !out.iter().any(|x| near(x, e)) {
            out.push(e.clone());
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.at));
    out
}

// ---- actions -------------------------------------------------------------------

/// The progress of an action (restart, stop or start) on `module`, judged
/// from what the spawn feed, the module roster and health checks have
/// reported since the action began at `started`. Returns its phase and, once
/// the outcome is known, whether it succeeded plus a line saying what
/// happened, such as the new pid or the time until the first healthy check.
pub fn action_progress(
    s: &State,
    kind: ActionKind,
    module: &str,
    before: Option<LiveProcess>,
    started: Timestamp,
    now: Timestamp,
) -> (ActionPhase, Option<(bool, String)>) {
    let exited = s.exits.get(module).is_some_and(|t| *t >= started);
    let current = s.processes.get(module).copied();
    let fresh =
        current.filter(|p| p.spawned_at >= started && before.is_none_or(|b| b.pid != p.pid));
    let roster_state = s
        .roster
        .value
        .iter()
        .flatten()
        .find(|e| e.module_id == module)
        .map(|e| e.state.as_str());
    let since = |t: Timestamp| crate::time::to_fixed(now.secs_since(t).max(0.0), 1);
    match kind {
        ActionKind::Stop => {
            let stopped = matches!(roster_state, Some("stopped" | "disabled")) && current.is_none();
            if stopped || (exited && current.is_none()) {
                let pid = before.map_or_else(String::new, |b| format!(" · pid {} exited", b.pid));
                return (
                    ActionPhase::Stopped,
                    Some((
                        true,
                        format!("{module} stopped{pid} after {} s", since(started)),
                    )),
                );
            }
            let phase = if exited {
                ActionPhase::Stopped
            } else {
                ActionPhase::Draining
            };
            (phase, None)
        }
        ActionKind::Restart | ActionKind::Start => {
            let verb = if kind == ActionKind::Restart {
                "restarted"
            } else {
                "started"
            };
            let Some(p) = fresh else {
                let phase = match (kind, exited) {
                    (ActionKind::Start, _) => ActionPhase::Asking,
                    (_, true) => ActionPhase::Stopped,
                    (_, false) => ActionPhase::Draining,
                };
                return (phase, None);
            };
            let health = s.health_of(module);
            let probed_after = health
                .and_then(|h| h.last_probe_ms)
                .map(|ms| Timestamp::from_unix_millis(ms as i64))
                .filter(|t| *t >= p.spawned_at);
            let starting = ActionPhase::Starting { pid: p.pid };
            match (health.map(|h| h.status), probed_after) {
                (Some(SupervisorHealthStatus::Ok), Some(t)) => (
                    starting,
                    Some((
                        true,
                        format!(
                            "{module} {verb} · pid {} · healthy after {} s",
                            p.pid,
                            crate::time::to_fixed(t.secs_since(p.spawned_at).max(0.0), 1)
                        ),
                    )),
                ),
                (Some(status), Some(_)) if status != SupervisorHealthStatus::Unknown => {
                    let word = match status {
                        SupervisorHealthStatus::Degraded => "degraded",
                        SupervisorHealthStatus::Failing => "failing",
                        _ => "unresponsive",
                    };
                    let detail = health
                        .and_then(|h| h.detail.as_deref())
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default();
                    (
                        starting,
                        Some((
                            false,
                            format!("{module} {verb} · pid {} · {word}{detail}", p.pid),
                        )),
                    )
                }
                // A module with no health check: once it has run for a while
                // the result says so rather than waiting for a check that
                // will never come.
                _ if now.secs_since(p.spawned_at) >= NO_HEALTH_GRACE_SECS
                    && roster_state == Some("running")
                    && health.is_none_or(|h| h.last_probe_ms.is_none()) =>
                {
                    (
                        starting,
                        Some((
                            true,
                            format!("{module} {verb} · pid {} · running, health unknown", p.pid),
                        )),
                    )
                }
                _ => (starting, None),
            }
        }
    }
}

/// How long a started or restarted module with no health check runs before
/// the action is reported as done with its health unknown.
pub const NO_HEALTH_GRACE_SECS: f64 = 15.0;
