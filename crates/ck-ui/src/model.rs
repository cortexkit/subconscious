//! What the screens show: a snapshot of the machine's state.
//!
//! Everything a screen displays that can change is a field here; renderers
//! hold only labels and headings. Each panel records when its data arrived,
//! so a renderer can tell fresh data from stale. Times are absolute
//! [`Timestamp`]s; renderers format them against a `now` and the host's UTC
//! offset.

#[cfg(feature = "fixture")]
use serde_json::{json, Value};

#[cfg(feature = "fixture")]
use crate::time::parse_duration;
use crate::time::{Timestamp, UtcOffset};

/// A module's (or a device's) state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    Ok,
    Degraded,
    Failed,
    Starting,
    Stopped,
    Unknown,
}

impl Status {
    pub fn parse(s: &str) -> Status {
        match s {
            "ok" => Status::Ok,
            "degraded" => Status::Degraded,
            "failed" => Status::Failed,
            "starting" => Status::Starting,
            "stopped" => Status::Stopped,
            _ => Status::Unknown,
        }
    }
}

/// What kind of text an error is, as the source typed it: a machine code
/// (from a `code`, `reason` or `state` field), a sentence meant for people
/// (a `message` or `detail`), or a string the source gave no type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FaultKind {
    Code,
    Sentence,
    #[default]
    Untyped,
}

/// A source's error: its text and what kind of text it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    pub text: String,
    pub kind: FaultKind,
}

impl Fault {
    pub fn code(text: impl Into<String>) -> Fault {
        Fault {
            text: text.into(),
            kind: FaultKind::Code,
        }
    }

    pub fn sentence(text: impl Into<String>) -> Fault {
        Fault {
            text: text.into(),
            kind: FaultKind::Sentence,
        }
    }
}

impl From<String> for Fault {
    fn from(text: String) -> Fault {
        Fault {
            text,
            kind: FaultKind::Untyped,
        }
    }
}

impl From<&str> for Fault {
    fn from(text: &str) -> Fault {
        Fault::from(text.to_string())
    }
}

impl PartialEq<&str> for Fault {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// One source's data, when it arrived, and how the source is doing.
#[derive(Clone, Debug, PartialEq)]
pub struct Panel<T> {
    /// When the data arrived. Meaningless until `loaded`.
    pub received_at: Timestamp,
    pub data: T,
    /// How often the source refreshes, in seconds. A panel past twice this
    /// age shows its age; past five times, it reads as a last good reading.
    /// `None` for data that has no schedule, which never goes stale by age.
    pub refresh_secs: Option<f64>,
    /// Why the source's latest attempt failed, in one line, while it is
    /// failing. The data, if any, is then the last good reading.
    pub error: Option<String>,
    /// What kind of text `error` is.
    pub error_kind: FaultKind,
    /// False until the source's first reading arrives. Until then `data`
    /// holds placeholders that renderers must show as unknown.
    pub loaded: bool,
}

/// How old a panel's data is against its refresh interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    /// Past twice the refresh interval: the age is worth showing.
    Aging,
    /// Past five times the refresh interval: the values are a last good
    /// reading, not the current state.
    Stale,
}

impl<T> Panel<T> {
    /// Data that arrived at `received_at`, on no schedule.
    pub fn new(received_at: Timestamp, data: T) -> Self {
        Panel {
            received_at,
            data,
            refresh_secs: None,
            error: None,
            error_kind: FaultKind::Untyped,
            loaded: true,
        }
    }

    /// A panel still waiting for its first reading, holding placeholders.
    pub fn waiting(data: T, refresh_secs: Option<f64>) -> Self {
        Panel {
            received_at: Timestamp::from_unix_millis(0),
            data,
            refresh_secs,
            error: None,
            error_kind: FaultKind::Untyped,
            loaded: false,
        }
    }

    pub fn freshness(&self, now: Timestamp) -> Freshness {
        let Some(every) = self.refresh_secs.filter(|r| *r > 0.0) else {
            return Freshness::Fresh;
        };
        if !self.loaded {
            return Freshness::Fresh;
        }
        let age = now.secs_since(self.received_at);
        if age > 5.0 * every {
            Freshness::Stale
        } else if age > 2.0 * every {
            Freshness::Aging
        } else {
            Freshness::Fresh
        }
    }
}

/// A health metric the module reports, in its own words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metric {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn parse(s: &str) -> LogLevel {
        match s {
            "warn" => LogLevel::Warn,
            "error" => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    /// When the line was written, if the source says. Unknown times are
    /// left blank rather than made up.
    pub at: Option<Timestamp>,
    pub level: LogLevel,
    pub message: String,
}

/// What a lifecycle action asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Restart,
    Stop,
    Start,
}

impl ActionKind {
    pub fn verb(self) -> &'static str {
        match self {
            ActionKind::Restart => "restart",
            ActionKind::Stop => "stop",
            ActionKind::Start => "start",
        }
    }
}

/// Where an action on a module has got to, as the daemon reports it.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionPhase {
    /// Sent; the daemon hasn't answered yet.
    Asking,
    /// The old process is finishing its work before it exits.
    Draining,
    /// The old process has exited and no new one is running yet.
    Stopped,
    /// A new process is running and hasn't passed a health check yet.
    Starting { pid: u32 },
}

/// An action in flight on one module.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleAction {
    pub kind: ActionKind,
    pub phase: ActionPhase,
    pub started_at: Timestamp,
}

impl ModuleAction {
    /// The phase in a few words, such as "draining" or "starting · pid 81022".
    pub fn phase_text(&self) -> String {
        match &self.phase {
            ActionPhase::Asking => match self.kind {
                ActionKind::Restart => "restarting".into(),
                ActionKind::Stop => "stopping".into(),
                ActionKind::Start => "starting".into(),
            },
            ActionPhase::Draining => "draining".into(),
            ActionPhase::Stopped => "stopped".into(),
            ActionPhase::Starting { pid } => format!("starting · pid {pid}"),
        }
    }
}

/// What the last action did, for the line that reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionResult {
    pub at: Timestamp,
    pub text: String,
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub id: String,
    pub status: Status,
    /// When the module entered its current status, if known.
    pub status_since: Option<Timestamp>,
    pub pid: Option<u32>,
    pub started_at: Option<Timestamp>,
    pub restarts: Option<u32>,
    /// The build the module declares ("unknown" when it declares none).
    pub build: String,
    pub commit: String,
    /// `None` until two readings of the same process are in, or when no
    /// process is running.
    pub cpu_percent: Option<f64>,
    pub memory_mb: Option<f64>,
    /// The module's own one-sentence health summary; empty when it gave none.
    pub summary: String,
    pub routes: Option<u32>,
    /// Who the module's open routes serve, one name per route; `None` when
    /// the daemon couldn't say.
    pub serving: Option<Vec<String>>,
    /// Why a restart is waiting, if one is.
    pub pending_restart: Option<String>,
    /// When the daemon last checked the module's health; `None` if never.
    pub health_checked_at: Option<Timestamp>,
    pub metrics: Vec<Metric>,
    /// CPU percent samples, oldest first.
    pub cpu_history: Vec<f64>,
    /// Memory samples in MB, oldest first.
    pub memory_history: Vec<f64>,
    /// The tail of the module's log, oldest first.
    pub log: Vec<LogLine>,
    /// An action on this module that is still in flight.
    pub action: Option<ModuleAction>,
    /// How long a restart or stop waits for work in flight before cutting
    /// it off, if the daemon says.
    pub drain_limit_secs: Option<f64>,
    /// Every metric the module reports, flattened to full dotted keys, for
    /// the all-metrics list.
    pub all_metrics: Vec<Metric>,
    /// Where the module is in its lifecycle, in a word ("Restarting",
    /// "Starting", "Stopped"), when that says more than its status does.
    pub lifecycle: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DaemonRunning {
    pub version: String,
    pub started_at: Option<Timestamp>,
    pub clients: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DaemonDown {
    /// When the daemon stopped, if anything says.
    pub stopped_at: Option<Timestamp>,
    /// Who stopped it and how, such as "by launchd, a normal shutdown".
    pub stopped_by: String,
    /// The daemon's last log line.
    pub last_line: String,
    /// The service manager's view, such as "loaded · restarts on failure".
    pub service: String,
    /// A line under the buttons: what starting the service did, or how ck is
    /// waiting for the daemon to come back.
    pub note: Option<String>,
    /// The end of the daemon's own log, oldest first, for "Show the log".
    pub log_tail: Vec<LogLine>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Daemon {
    Running(DaemonRunning),
    Down(DaemonDown),
}

/// The computer ck runs on: what the top bar calls it, its machine id, its
/// UTC offset for clock times, and its memory, disk and load.
#[derive(Clone, Debug, PartialEq)]
pub struct Host {
    /// How the top bar names this machine, such as "This computer".
    pub label: String,
    pub machine_id: Option<String>,
    pub utc_offset: UtcOffset,
    /// The home folder of the person running ck, so paths under it can be
    /// shown as `~/…`; `None` when it isn't known.
    pub home: Option<String>,
    pub memory_used_gb: Option<f64>,
    pub memory_total_gb: Option<f64>,
    pub disk_used_gb: Option<f64>,
    pub disk_total_gb: Option<f64>,
    /// Load averages over 1, 5 and 15 minutes; `None` where the system
    /// keeps none.
    pub load: Option<[f64; 3]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BuildServer {
    pub busy: Option<u32>,
    pub slots: Option<u32>,
    pub queued: Option<u32>,
    pub wait_secs: Option<f64>,
    /// Load samples in percent, oldest first. A NaN is a gap: no reading
    /// for that minute, which is not a zero.
    pub load_history: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Backups {
    pub last_upload_at: Option<Timestamp>,
    pub uploading: bool,
    /// Ok when uploads are current, degraded when behind.
    pub status: Status,
    /// The module's own sentence about backups, shown when nothing is
    /// uploading.
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    pub name: String,
    pub used: f64,
    pub resets_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    /// The account's name: its email, else its account id (cut to eight
    /// characters when it is a UUID), else its credential source, else
    /// "account" (`ck_ui::quota::account_name`).
    pub who: String,
    /// The name shown instead of `who` while account identifiers are hidden:
    /// the pseudonym `ck quota --redact` prints.
    pub redacted: String,
    /// The values `ck quota --redact` hides for this account: its email, its
    /// account id (whole, and as `who` shortens it) and its organization's
    /// name. While account identifiers are hidden, any of them that appears
    /// in an error sentence is replaced with `redacted`
    /// (`crate::quota::mask_identifiers`).
    pub identifiers: Vec<String>,
    pub windows: Vec<UsageWindow>,
    pub note: String,
    /// Why this account's reading failed, if it did. A failing account has
    /// no usage, which is not 0%.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub name: String,
    pub window: String,
    /// Accounts' worth of capacity used, out of `of`; `None` when no
    /// account could be read.
    pub used: Option<f64>,
    pub of: u32,
    /// "account" or "accounts".
    pub unit: String,
    pub resets_at: Option<Timestamp>,
    pub accounts: Vec<Account>,
    /// Why the provider can't be read at all, if it can't.
    pub error: Option<String>,
    /// What kind of text `error` is.
    pub error_kind: FaultKind,
    /// What is left of a provider's credit pools, such as "$983.26 of
    /// $1,000.00 left · 5 pools", for a provider whose accounts report credit
    /// instead of usage windows.
    pub balance: Option<String>,
    /// When the soonest of those pools expires.
    pub balance_expires: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub providers: Vec<Provider>,
    pub models_priced: Option<u64>,
    /// How many models the pricing module knows of, priced or not.
    pub models_counted: Option<u64>,
    pub models_checked_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivityEntry {
    pub at: Timestamp,
    pub who: String,
    pub subject: String,
    pub what: String,
    pub kind: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessEventKind {
    Started,
    Exited,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessEvent {
    pub at: Timestamp,
    pub module: String,
    pub what: String,
    pub kind: ProcessEventKind,
}

/// Which row of the Activity list something is, stable while rows are
/// added and dropped around it, so a selection can follow its row.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActivityKey {
    /// A ledger entry, by the identity the daemon gives it.
    Entry { incarnation: String, seq: u64 },
    /// A process exit, by its module and time.
    Exit { module: String, at: Timestamp },
    /// The "Entries here are no longer kept" label below the entry with
    /// this seq.
    Gap { below_seq: u64 },
}

/// What an Activity row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityRowKind {
    /// One ledger entry, or a restart and its start read as one.
    Entry,
    /// A process exit from the spawn feed or the daemon's retained exits.
    Exit,
    /// A dim label where entries between two loaded ranges are no longer
    /// kept. It counts toward no total.
    Gap,
}

/// One row of the Activity list.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivityRow {
    pub key: ActivityKey,
    /// The other entry a merged restart row stands for (its start), so a
    /// selection on either finds the row.
    pub partner: Option<ActivityKey>,
    pub row: ActivityRowKind,
    pub at: Timestamp,
    /// The module the row is about.
    pub subject: String,
    /// What happened, without the subject: "restarted on build 9d8a178c".
    pub what: String,
    /// Who asked for it; `None` for exits and labels.
    pub who: Option<String>,
    /// Who asked for it as the detail pane's By row words it, when that
    /// says more than `who`. Only the design fixture has such wording (its
    /// "ck on this computer"); the live app leaves it `None`, and the pane reads
    /// `who`.
    pub by: Option<String>,
    /// Whether the actor carries an agent id: the Agents chip lists these.
    pub by_agent: bool,
    /// The entry's kind as recorded ("subc.module_restarted"); `None` for
    /// exits and labels.
    pub kind: Option<String>,
    pub seq: Option<u64>,
    /// The build the row's start entry names: "<old 8> → <new 8>" when both
    /// its evidences have a digest, else the new digest's 8 alone. Only when
    /// the new evidence has a digest; a merged restart row takes it from
    /// its start.
    pub build: Option<String>,
    /// The entry's details as recorded; `None` for exits and labels.
    pub details: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ActivityRow {
    /// Whether `key` is this row: its own entry or the one merged into it.
    pub fn is(&self, key: &ActivityKey) -> bool {
        self.key == *key || self.partner.as_ref() == Some(key)
    }

    /// Whether the row counts toward the list's totals and the loaded cap:
    /// every entry and exit row, never a label.
    pub fn counts(&self) -> bool {
        self.row != ActivityRowKind::Gap
    }
}

/// The Activity list as loaded: the ledger merged with process exits,
/// newest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActivityLog {
    pub rows: Vec<ActivityRow>,
    /// Whether there is nothing older to load: the last page read was the
    /// start of retained history (an empty ledger included).
    pub history_exhausted: bool,
    /// Whether newer entries exist than the first loaded row, dropped while
    /// older pages were read; they are read again when the view returns to
    /// the top.
    pub newer_unloaded: bool,
}

impl ActivityLog {
    /// The position of the row `key` names, whether it is the row's own
    /// entry or the one merged into it.
    pub fn position(&self, key: &ActivityKey) -> Option<usize> {
        self.rows.iter().position(|r| r.is(key))
    }

    /// How many rows count toward totals: entries and exits, not labels.
    pub fn counted(&self) -> usize {
        self.rows.iter().filter(|r| r.counts()).count()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    pub name: String,
    pub project: String,
    /// "working", how long it has been idle ("idle 4m"), or "unknown".
    pub state: String,
    pub machine: String,
    pub doing: Option<String>,
    /// The role the identity source gives the agent; `None` when it gives
    /// none.
    pub role: Option<String>,
    /// How many of the agent's questions wait for an answer, as the state
    /// source counts them; `None` when it doesn't say.
    pub pending_asks: Option<u32>,
}

impl Agent {
    pub fn working(&self) -> bool {
        self.state == "working"
    }

    pub fn state_known(&self) -> bool {
        self.state != "unknown"
    }
}

/// A workspace and how many of the listed agents belong to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub agents: u32,
    /// How many projects the workspace holds. No live source counts them
    /// per workspace, so the live app leaves this `None` and draws nothing
    /// for it.
    pub projects: Option<u32>,
}

/// A machine that holds agents, and how many of the listed agents it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub name: String,
    pub agents: u32,
    /// Whether the machine is reachable now. No live source reports it, so
    /// the live app leaves this `None` and draws no dot for it.
    pub online: Option<bool>,
}

/// When each source joined into the agents list last reported, and how it
/// is doing, so each column can age and dim by its own source: a fresh
/// identity reply never makes an old state or place look current.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSources {
    /// Names, projects, roles and workspaces.
    pub identity: Panel<()>,
    /// Working or idle, what each agent is doing, its waiting questions.
    pub state: Panel<()>,
    /// Which machine holds each agent.
    pub places: Panel<()>,
    /// The fleet's workspace and project totals.
    pub counts: Panel<()>,
}

impl AgentSources {
    /// Every source read at `at`, on no schedule.
    pub fn read_at(at: Timestamp) -> AgentSources {
        AgentSources {
            identity: Panel::new(at, ()),
            state: Panel::new(at, ()),
            places: Panel::new(at, ()),
            counts: Panel::new(at, ()),
        }
    }
}

impl Default for AgentSources {
    /// No source has reported yet.
    fn default() -> AgentSources {
        AgentSources {
            identity: Panel::waiting((), None),
            state: Panel::waiting((), None),
            places: Panel::waiting((), None),
            counts: Panel::waiting((), None),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Agents {
    pub list: Vec<Agent>,
    pub working: u32,
    pub machines: Option<u32>,
    pub workspaces: Option<u32>,
    pub projects: Option<u32>,
    /// One row per workspace the listed agents belong to.
    pub workspace_list: Vec<Workspace>,
    /// One row per machine holding any of the listed agents.
    pub machine_list: Vec<Machine>,
    pub sources: AgentSources,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub name: String,
    /// A shorter name for tight rows.
    pub short_name: String,
    pub kind: String,
    pub status: Status,
    pub path: String,
    pub seen: String,
    pub feeds: String,
    pub fingerprint: Option<String>,
    pub trust: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Devices {
    pub list: Vec<Device>,
    pub online: u32,
    pub lan_addrs: Option<Vec<String>>,
    pub rendezvous: Option<String>,
    pub watch_ops_offered: Option<u64>,
    /// The access groups a new phone can be paired with.
    pub access_groups: AccessGroups,
}

/// A group of operations a paired device may call: one key of the
/// `expose_profiles` map in this computer's `fed-profile.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessGroup {
    pub name: String,
    /// How many operations the group allows; `None` when callosum doesn't
    /// say.
    pub operation_count: Option<u64>,
}

/// What `callosum.devices_read` says about the access groups
/// (`self.expose_profiles`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessGroups {
    /// The groups, in callosum's order.
    Listed(Vec<AccessGroup>),
    /// The reply has no list: callosum predates it.
    Unlisted,
    /// The list is there but not in a shape ck can read, so ck offers no
    /// group rather than guess one.
    Malformed,
}

impl Default for AccessGroups {
    /// Nothing read: no group can be offered.
    fn default() -> AccessGroups {
        AccessGroups::Malformed
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Approvals {
    /// `None` while nothing can say how many are waiting.
    pub waiting: Option<u32>,
}

/// An entry in the command palette and the area it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteCommand {
    pub title: String,
    pub area: String,
}

/// One module's full log, for the log view.
#[derive(Clone, Debug, PartialEq)]
pub struct LogView {
    pub module: String,
    /// Oldest first.
    pub lines: Vec<LogLine>,
    /// Where the lines come from, for the footer.
    pub source: String,
}

/// Everything the app shows, as of the times its panels record.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub settings: crate::live::settings::SettingsReading,
    pub host: Panel<Host>,
    pub daemon: Panel<Daemon>,
    pub modules: Panel<Vec<Module>>,
    pub build_server: Panel<BuildServer>,
    pub backups: Panel<Backups>,
    pub usage: Panel<Usage>,
    pub activity: Panel<Vec<ActivityEntry>>,
    /// The Activity screen's list: the ledger as loaded, merged with process
    /// exits.
    pub activity_log: Panel<ActivityLog>,
    pub process_events: Panel<Vec<ProcessEvent>>,
    pub agents: Panel<Agents>,
    pub devices: Panel<Devices>,
    pub approvals: Panel<Approvals>,
    pub commands: Vec<PaletteCommand>,
    /// The log the log view shows, when one is open.
    pub log_view: Option<Panel<LogView>>,
    /// What the most recent action did.
    pub last_result: Option<ActionResult>,
    /// Which keys and screens the app serves; it advertises nothing else.
    pub serves: Serves,
    /// The latest results of the Add a phone exchange, each with its
    /// sequence number, oldest first. The app applies those newer than the
    /// last it applied, so a result is never lost when snapshots come
    /// faster than frames.
    pub pairing: Vec<(u64, crate::pairing::flow::Event)>,
    /// The latest Save previews' replies, each with its sequence number,
    /// oldest first, applied like `pairing`'s results.
    pub previews: Vec<(u64, crate::live::settings::PreviewReply)>,
    pub applies: Vec<(u64, crate::live::settings::ApplyReply)>,
    /// Counts the connections the live layer has made to the daemon. It
    /// moves on every reconnect, which tells the app that a preview it is
    /// still waiting for may never be answered and must be asked for again.
    pub daemon_connection: u64,
    /// Settings → Updates: the session's update check and the run started
    /// from it.
    pub upgrade: crate::upgrade::Updates,
}

/// The app's tabs, in the order the tab bar draws them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tab {
    Home,
    Modules,
    Activity,
    Usage,
    Agents,
    Devices,
    Settings,
}

impl Tab {
    /// Every tab, in bar order.
    pub const ALL: [Tab; 7] = [
        Tab::Home,
        Tab::Modules,
        Tab::Activity,
        Tab::Usage,
        Tab::Agents,
        Tab::Devices,
        Tab::Settings,
    ];

    /// The tab's number, which is also its key: Home 1 through Settings 7.
    /// A tab keeps its number while others are hidden, so a key always
    /// means the same tab.
    pub const fn number(self) -> usize {
        self as usize + 1
    }

    /// The key that switches to the tab.
    pub fn key(self) -> char {
        char::from(b'0' + self.number() as u8)
    }

    /// The tab a number key names, whether or not that tab is showing.
    pub fn from_key(ch: char) -> Option<Tab> {
        let n = ch.to_digit(10)? as usize;
        Tab::ALL.get(n.checked_sub(1)?).copied()
    }

    pub const fn name(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Modules => "Modules",
            Tab::Activity => "Activity",
            Tab::Usage => "Usage",
            Tab::Agents => "Agents",
            Tab::Devices => "Devices",
            Tab::Settings => "Settings",
        }
    }

    /// The tab with this name, as the palette and the bar spell it.
    pub fn from_name(name: &str) -> Option<Tab> {
        Tab::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The module whose data the tab shows, when the tab has nothing to show
    /// without it: the tab is hidden while that module isn't in the
    /// supervisor's roster. Home, Modules, Activity and Settings come from
    /// the daemon itself and need none.
    pub fn needs_module(self) -> Option<&'static str> {
        use crate::live::modules;
        match self {
            Tab::Usage => Some(modules::USAGE),
            Tab::Agents => Some(modules::IDENTITY),
            Tab::Devices => Some(modules::DEVICES),
            Tab::Home | Tab::Modules | Tab::Activity | Tab::Settings => None,
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// A set of tabs, iterated in bar order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TabSet(u8);

impl TabSet {
    pub const EMPTY: TabSet = TabSet(0);
    pub const ALL: TabSet = TabSet::of(&Tab::ALL);

    pub const fn of(tabs: &[Tab]) -> TabSet {
        let mut bits = 0;
        let mut i = 0;
        while i < tabs.len() {
            bits |= tabs[i].bit();
            i += 1;
        }
        TabSet(bits)
    }

    pub const fn contains(self, tab: Tab) -> bool {
        self.0 & tab.bit() != 0
    }

    pub const fn with(self, tab: Tab) -> TabSet {
        TabSet(self.0 | tab.bit())
    }

    pub const fn without(self, tab: Tab) -> TabSet {
        TabSet(self.0 & !tab.bit())
    }

    /// The tabs in both sets.
    pub const fn and(self, other: TabSet) -> TabSet {
        TabSet(self.0 & other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    pub fn iter(self) -> impl Iterator<Item = Tab> {
        Tab::ALL.into_iter().filter(move |t| self.contains(*t))
    }
}

impl FromIterator<Tab> for TabSet {
    fn from_iter<I: IntoIterator<Item = Tab>>(iter: I) -> TabSet {
        iter.into_iter().fold(TabSet::EMPTY, TabSet::with)
    }
}

/// The keys and screens a build of the app serves. Hints, the keys page and
/// the tab bar list only these, so nothing on screen offers an action the
/// app can't take. The fixture serves the whole design; the live app
/// serves what has been built so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Serves {
    /// The tabs that have a screen. Whether a served tab shows also depends
    /// on the roster: see [`Snapshot::visible_tabs`].
    pub tabs: TabSet,
    /// `e`: the Settings screen.
    pub settings: bool,
    /// `⏎` and `s` in the Save overlay: writing `subc.jsonc`. Without it
    /// the overlay only reviews the change and offers `esc` alone.
    pub save: bool,
    /// `w`: restart without downtime (a blue/green swap).
    pub swap: bool,
    /// The tabs whose list `/` filters. Each tab's filter is served on its
    /// own, so serving one tab's filter never advertises another's.
    pub list_filter: TabSet,
    /// Space: selecting several list rows.
    pub multi_select: bool,
    /// Double-click to open.
    pub double_click: bool,
    /// `ckni`, the plain-output twin of `ck`.
    pub ckni: bool,
    /// A source for approvals waiting on this machine.
    pub approvals: bool,
    /// `m`: the selected module's full metrics.
    pub all_metrics: bool,
    /// `⏎` on Usage, an account's detail, and `m` there, the model catalog.
    /// The design draws both keys but no layout for either.
    pub usage_detail: bool,
    /// `r` on Usage: refresh now. The quota module offers no refresh
    /// operation the app could call.
    pub usage_refresh: bool,
    /// `⏎` on Agents: an agent's detail. The design draws the key but no
    /// layout for it.
    pub agent_detail: bool,
    /// `p` on Agents: the projects view. The design draws the key but no
    /// layout for it.
    pub agent_projects: bool,
}

impl Serves {
    /// Everything the design draws.
    pub const DESIGN: Serves = Serves {
        tabs: TabSet::ALL,
        settings: true,
        save: true,
        swap: true,
        // The design draws a filter on these lists, and none on Home,
        // Usage or Devices.
        list_filter: TabSet::of(&[Tab::Modules, Tab::Activity, Tab::Agents, Tab::Settings]),
        multi_select: true,
        double_click: true,
        ckni: true,
        approvals: true,
        // The design draws no all-metrics list; the live app has one.
        all_metrics: false,
        usage_detail: true,
        usage_refresh: true,
        agent_detail: true,
        agent_projects: true,
    };

    /// Built screens and their supported actions, independent of the roster.
    pub const LIVE: Serves = Serves {
        tabs: TabSet::of(&[
            Tab::Home,
            Tab::Modules,
            Tab::Activity,
            Tab::Usage,
            Tab::Agents,
            Tab::Devices,
            Tab::Settings,
        ]),
        settings: true,
        save: true,
        swap: false,
        list_filter: TabSet::of(&[Tab::Activity, Tab::Agents, Tab::Settings]),
        multi_select: false,
        double_click: false,
        ckni: false,
        approvals: false,
        all_metrics: true,
        usage_detail: false,
        usage_refresh: false,
        agent_detail: false,
        agent_projects: false,
    };
}

impl Snapshot {
    /// The tabs that show: the served tabs whose module, if they need one
    /// (see [`Tab::needs_module`]), is in the supervisor's roster, enabled
    /// or not, running or not. Until the first roster arrives, no tab that
    /// needs a module shows. The tab bar, the number keys, the palette and
    /// the keys page all follow this one set.
    pub fn visible_tabs(&self) -> TabSet {
        self.serves
            .tabs
            .iter()
            .filter(|t| match t.needs_module() {
                None => true,
                Some(id) => self.modules.loaded && self.modules.data.iter().any(|m| m.id == id),
            })
            .collect()
    }

    /// Modules whose status isn't ok, in roster order.
    pub fn attention(&self) -> Vec<&Module> {
        self.modules
            .data
            .iter()
            .filter(|m| m.status != Status::Ok)
            .collect()
    }

    /// The fixture: the frozen machine state the golden frames in
    /// `tests/golden/` were drawn from, as of [`Snapshot::fixture_now`].
    #[cfg(feature = "fixture")]
    pub fn fixture() -> Snapshot {
        fixture::build(FIXTURE_JSON).expect("the built-in fixture parses")
    }

    /// The fixture with the daemon stopped.
    #[cfg(feature = "fixture")]
    pub fn fixture_daemon_down() -> Snapshot {
        let mut s = Snapshot::fixture();
        let now = Snapshot::fixture_now();
        s.daemon = Panel::new(
            now,
            Daemon::Down(DaemonDown {
                stopped_at: Some(fixture::today(now, "09:29:51")),
                stopped_by: "by launchd, a normal shutdown".into(),
                last_line: "subc daemon stopping: SIGTERM".into(),
                service: "loaded · restarts on failure".into(),
                note: None,
                log_tail: Vec::new(),
            }),
        );
        s
    }

    /// The fixture with broca's log open in the log view.
    #[cfg(feature = "fixture")]
    pub fn fixture_logs() -> Snapshot {
        let mut s = Snapshot::fixture();
        let now = Snapshot::fixture_now();
        let broca = s
            .modules
            .data
            .iter()
            .find(|m| m.id == "broca")
            .expect("the fixture has broca");
        s.log_view = Some(Panel::new(
            now,
            LogView {
                module: broca.id.clone(),
                lines: broca.log.clone(),
                source: "tail of broca.2026-10-20.log".into(),
            },
        ));
        s
    }

    /// The instant the fixture describes.
    #[cfg(feature = "fixture")]
    pub fn fixture_now() -> Timestamp {
        static NOW: std::sync::OnceLock<Timestamp> = std::sync::OnceLock::new();
        *NOW.get_or_init(|| {
            let v: Value = serde_json::from_str(FIXTURE_JSON).expect("fixture JSON");
            Timestamp::parse_utc(v["now_utc"].as_str().unwrap_or_default()).expect("fixture time")
        })
    }
}

// The fixture is test and demo data. It sits behind the `fixture` feature so
// that normal builds don't carry it.
#[cfg(feature = "fixture")]
const FIXTURE_JSON: &str = include_str!("../tests/golden/fixture.json");

#[cfg(feature = "fixture")]
mod fixture {
    //! Builds the fixture snapshot from `tests/golden/fixture.json`, the data
    //! the golden frames were drawn from. The file holds clock times
    //! ("18:20") and durations ("up" seconds, "9h 25m" until a reset); they
    //! become absolute times relative to its `now_utc`. Facts the file
    //! doesn't hold as data (daemon version, client count, build server
    //! queue and the like) are filled in here with the values the golden
    //! frames show.

    use super::*;

    fn s(v: &Value) -> String {
        v.as_str().unwrap_or_default().to_string()
    }

    fn f(v: &Value) -> f64 {
        v.as_f64().unwrap_or(0.0)
    }

    fn u(v: &Value) -> u32 {
        v.as_f64().unwrap_or(0.0) as u32
    }

    fn arr(v: &Value) -> &[Value] {
        v.as_array().map_or(&[], Vec::as_slice)
    }

    fn floats(v: &Value) -> Vec<f64> {
        arr(v).iter().map(f).collect()
    }

    /// `HH:MM[:SS[.mmm]]` on the UTC day of `now`.
    pub(super) fn today(now: Timestamp, clock: &str) -> Timestamp {
        on_day(now, 0, clock)
    }

    /// `HH:MM[:SS[.mmm]]` on the UTC day `days_back` days before `now`'s.
    pub(super) fn on_day(now: Timestamp, days_back: i64, clock: &str) -> Timestamp {
        let day_start = now.unix_millis().div_euclid(86_400_000) * 86_400_000;
        let (hms, ms) = clock.split_once('.').unwrap_or((clock, "0"));
        let mut parts = hms.split(':').map(|p| p.parse::<i64>().unwrap_or(0));
        let h = parts.next().unwrap_or(0);
        let m = parts.next().unwrap_or(0);
        let sec = parts.next().unwrap_or(0);
        let ms: i64 = ms.parse().unwrap_or(0);
        Timestamp::from_unix_millis(
            day_start - days_back * 86_400_000 + ((h * 60 + m) * 60 + sec) * 1000 + ms,
        )
    }

    /// The Activity screen's rows: today's entries, the ones the Activity
    /// frame draws. The file gives each a clock time, a module, what
    /// happened, who did it and a short kind word ("module", "approval",
    /// "config", "agent", "flow"). The rest is the frame's: every entry
    /// recorded at 36 seconds past its minute, seqs counting down from
    /// 48,213, the ledger kind each kind word stands for, "ck on this computer"
    /// as the By row for ck, and for a restart on a build the old build
    /// 7c3a47b9 with the frame's details.
    fn activity_log(v: &Value, now: Timestamp) -> ActivityLog {
        let rows = arr(&v["activity"])
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let subject = s(&a["mod"]);
                let what = s(&a["what"]);
                let who = s(&a["who"]);
                let word = s(&a["kind"]);
                let kind = match word.as_str() {
                    "module" => "subc.module_restarted".to_string(),
                    "approval" => "cingulate.consent_answered".to_string(),
                    "config" => "subc.config_changed".to_string(),
                    other => format!("{subject}.{other}"),
                };
                let digest = what.strip_prefix("restarted on build ").map(str::to_string);
                let mut details = serde_json::Map::new();
                if let Some(d) = &digest {
                    details.insert("old_evidence".into(), Value::from("7c3a47b9…"));
                    details.insert("new_evidence".into(), Value::from(format!("{d}…")));
                    details.insert("drain_ms".into(), Value::from(412));
                }
                let seq = 48_213 - i as u64;
                ActivityRow {
                    key: ActivityKey::Entry {
                        incarnation: "fixture".into(),
                        seq,
                    },
                    partner: None,
                    row: ActivityRowKind::Entry,
                    at: today(now, &format!("{}:36", a["t"].as_str().unwrap_or("00:00"))),
                    by: (who == "ck").then(|| "ck on this computer".to_string()),
                    by_agent: word == "agent",
                    who: Some(who),
                    kind: Some(kind),
                    seq: Some(seq),
                    build: digest.map(|d| format!("7c3a47b9 → {d}")),
                    details: Some(details),
                    subject,
                    what,
                }
            })
            .collect();
        ActivityLog {
            rows,
            history_exhausted: false,
            newer_unloaded: false,
        }
    }

    fn duration_from(now: Timestamp, text: &str) -> Option<Timestamp> {
        parse_duration(text).map(|secs| now.add_secs(secs))
    }

    /// Settings → Updates as the Upgrade frame draws it: the file's plan
    /// rows, checked twelve minutes before `now`, the frame's "Checked 12
    /// minutes ago". A row replaces its component when its versions differ,
    /// and the row named "daemon …" is the daemon.
    fn upgrade(v: &Value, now: Timestamp) -> crate::upgrade::Updates {
        use crate::upgrade::{Checked, PlanRow, Updates, UpgradeCheck};
        let rows = arr(&v["upgrade"])
            .iter()
            .map(|r| PlanRow {
                component: s(&r[0]),
                installed: s(&r[1]),
                new: s(&r[2]),
                what: s(&r[3]),
                replaces: r[1] != r[2],
                daemon: s(&r[0]).starts_with("daemon"),
            })
            .collect();
        Updates {
            served: true,
            check: Some(Checked {
                at: now.add_secs(-12.0 * 60.0),
                result: Ok(UpgradeCheck {
                    rows,
                    refusal: None,
                    stages: Vec::new(),
                }),
            }),
            run: None,
        }
    }

    pub(super) fn build(json: &str) -> Result<Snapshot, String> {
        let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let now = Timestamp::parse_utc(v["now_utc"].as_str().ok_or("now_utc")?)
            .ok_or("now_utc format")?;

        // The file's log lines belong to broca; every other module's log is
        // just its two most recent health checks.
        let broca_log: Vec<LogLine> = arr(&v["log_lines"])
            .iter()
            .map(|l| LogLine {
                at: Some(today(now, l[0].as_str().unwrap_or("00:00"))),
                level: LogLevel::parse(l[1].as_str().unwrap_or("info")),
                message: s(&l[2]),
            })
            .collect();
        let broca_metrics: Vec<Metric> = arr(&v["broca_detail"]["metrics"])
            .iter()
            .map(|m| Metric {
                label: s(&m[0]),
                value: s(&m[1]),
            })
            .collect();

        let modules = arr(&v["modules"])
            .iter()
            .map(|m| {
                let id = s(&m["id"]);
                let status = Status::parse(m["status"].as_str().unwrap_or(""));
                let status_since = match id.as_str() {
                    "engram" => Some(today(now, "18:20")),
                    _ if status != Status::Ok => Some(today(now, "09:30")),
                    _ => None,
                };
                let (metrics, log) = if id == "broca" {
                    (broca_metrics.clone(), broca_log.clone())
                } else {
                    let metric = |label: &str, value: String| Metric {
                        label: label.into(),
                        value,
                    };
                    let health_line = |clock: &str| LogLine {
                        at: Some(today(now, clock)),
                        level: LogLevel::Info,
                        message: "health check ok".into(),
                    };
                    (
                        vec![
                            metric("Routes", u(&m["routes"]).to_string()),
                            metric("Restarts", u(&m["restarts"]).to_string()),
                        ],
                        vec![health_line("18:39:12"), health_line("18:44:12")],
                    )
                };
                Module {
                    status,
                    status_since,
                    pid: Some(u(&m["pid"])),
                    started_at: Some(now.add_secs(-f(&m["up"]))),
                    restarts: Some(u(&m["restarts"])),
                    build: s(&m["build"]),
                    commit: s(&m["commit"]),
                    cpu_percent: Some(f(&m["cpu"])),
                    memory_mb: Some(f(&m["mem"])),
                    summary: s(&m["summary"]),
                    routes: Some(u(&m["routes"])),
                    serving: None,
                    pending_restart: m["pending"].as_str().map(str::to_string),
                    health_checked_at: Some(now.add_secs(-1.0)),
                    action: None,
                    drain_limit_secs: None,
                    all_metrics: Vec::new(),
                    lifecycle: None,
                    metrics,
                    cpu_history: floats(&m["cpu_history"]),
                    memory_history: floats(&m["mem_history"]),
                    log,
                    id,
                }
            })
            .collect();

        let host = &v["host"];
        let load: Vec<f64> = arr(&host["load"])
            .iter()
            .map(|l| l.as_str().and_then(|t| t.parse().ok()).unwrap_or(0.0))
            .collect();
        let host = Host {
            label: "This computer".into(),
            machine_id: Some("4f1a9c2e7b3d40c8a6e5d1b2c3f40912".into()),
            utc_offset: UtcOffset::UTC,
            home: None,
            memory_used_gb: Some(f(&host["memUsed"])),
            memory_total_gb: Some(f(&host["memTotal"])),
            disk_used_gb: Some(f(&host["diskUsed"])),
            disk_total_gb: Some(f(&host["diskTotal"])),
            load: Some([
                load.first().copied().unwrap_or(0.0),
                load.get(1).copied().unwrap_or(0.0),
                load.get(2).copied().unwrap_or(0.0),
            ]),
        };

        let providers = arr(&v["quota"])
            .iter()
            .map(|q| Provider {
                name: s(&q["name"]),
                window: s(&q["window"]),
                used: Some(f(&q["used"])),
                of: u(&q["of"]),
                unit: s(&q["unit"]),
                resets_at: duration_from(now, q["reset"].as_str().unwrap_or("")),
                error: None,
                error_kind: FaultKind::Untyped,
                balance: None,
                balance_expires: None,
                accounts: arr(&q["accounts"])
                    .iter()
                    .map(|a| Account {
                        who: s(&a["who"]),
                        // The fixture names accounts only by email, so the
                        // pseudonym is the one `ck quota --redact` derives
                        // from an email.
                        redacted: crate::quota::account_name(
                            &json!({ "accountInfo": { "email": s(&a["who"]) } }),
                            true,
                        ),
                        identifiers: vec![s(&a["who"])],
                        note: s(&a["note"]),
                        error: None,
                        windows: arr(&a["windows"])
                            .iter()
                            .map(|w| UsageWindow {
                                name: s(&w[0]),
                                used: f(&w[1]),
                                resets_at: duration_from(now, w[2].as_str().unwrap_or("")),
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect();

        let entry = |a: &Value, days_back: i64| ActivityEntry {
            at: on_day(now, days_back, a["t"].as_str().unwrap_or("00:00")),
            who: s(&a["who"]),
            subject: s(&a["mod"]),
            what: s(&a["what"]),
            kind: s(&a["kind"]),
        };
        let activity = arr(&v["activity"])
            .iter()
            .map(|a| entry(a, 0))
            .chain(arr(&v["activity_earlier"]).iter().map(|a| entry(a, 1)))
            .collect();

        let process_events = arr(&v["process_events"])
            .iter()
            .map(|e| ProcessEvent {
                at: today(now, e[0].as_str().unwrap_or("00:00")),
                module: s(&e[1]),
                what: s(&e[2]),
                // The file marks exits with a fourth "d" (dim) element,
                // because exits are drawn quieter than starts.
                kind: if e[3].as_str() == Some("d") {
                    ProcessEventKind::Exited
                } else {
                    ProcessEventKind::Started
                },
            })
            .collect();

        let doing = &v["doing"];
        let list: Vec<Agent> = arr(&v["agents"])
            .iter()
            .map(|a| {
                let name = s(&a[0]);
                Agent {
                    project: s(&a[1]),
                    state: s(&a[2]),
                    machine: s(&a[3]),
                    doing: match doing[name.as_str()].as_str() {
                        Some("—") => None,
                        Some(d) => Some(d.to_string()),
                        // The design draws an agent its doing table leaves
                        // out with a plain "—" as text, not the quiet
                        // mark for doing nothing.
                        None => Some("—".to_string()),
                    },
                    name,
                    role: None,
                    pending_asks: None,
                }
            })
            .collect();
        // The machines the listed agents live on, most agents first, each
        // drawn online as the design shows them.
        let mut machine_list: Vec<Machine> = Vec::new();
        for a in &list {
            match machine_list.iter_mut().find(|m| m.name == a.machine) {
                Some(m) => m.agents += 1,
                None => machine_list.push(Machine {
                    name: a.machine.clone(),
                    agents: 1,
                    online: Some(true),
                }),
            }
        }
        machine_list.sort_by(|a, b| b.agents.cmp(&a.agents).then(a.name.cmp(&b.name)));
        let agents = Agents {
            list,
            working: 5,
            machines: Some(2),
            workspaces: Some(2),
            projects: Some(35),
            // The design's workspace rows, which the fixture file doesn't
            // carry: they count the whole fleet, not only the agents listed.
            workspace_list: vec![
                Workspace {
                    name: "CortexKit".into(),
                    agents: 29,
                    projects: Some(31),
                },
                Workspace {
                    name: "Scratch".into(),
                    agents: 3,
                    projects: Some(4),
                },
            ],
            machine_list,
            sources: AgentSources::read_at(now),
        };

        let devices = Devices {
            list: arr(&v["devices"])
                .iter()
                .map(|d| {
                    let name = s(&d["name"]);
                    Device {
                        short_name: name.strip_prefix("CortexKit ").unwrap_or(&name).to_string(),
                        kind: s(&d["kind"]),
                        status: Status::parse(d["status"].as_str().unwrap_or("")),
                        path: s(&d["path"]),
                        seen: s(&d["seen"]),
                        feeds: s(&d["feeds"]),
                        fingerprint: None,
                        trust: None,
                        name,
                    }
                })
                .collect(),
            online: 2,
            lan_addrs: Some(vec!["192.168.1.20:7841".into()]),
            rendezvous: Some("rdv.cortexkit.io".into()),
            watch_ops_offered: Some(10),
            access_groups: AccessGroups::Listed(
                arr(&v["pairing"]["groups"])
                    .iter()
                    .map(|g| AccessGroup {
                        name: s(&g[0]),
                        operation_count: g[1].as_u64(),
                    })
                    .collect(),
            ),
        };

        let commands = arr(&v["commands"])
            .iter()
            .map(|c| PaletteCommand {
                title: s(&c[0]),
                area: s(&c[1]),
            })
            .collect();

        Ok(Snapshot {
            settings: crate::live::settings::SettingsReading::default(),
            host: Panel::new(now, host),
            daemon: Panel::new(
                now,
                Daemon::Running(DaemonRunning {
                    version: "0.36.17".into(),
                    started_at: Some(now.add_secs(-9.25 * 3600.0)),
                    clients: Some(366),
                }),
            ),
            modules: Panel::new(now, modules),
            build_server: Panel::new(
                now,
                BuildServer {
                    busy: Some(80),
                    slots: Some(80),
                    queued: Some(21),
                    wait_secs: Some(180.0),
                    load_history: floats(&v["build_load_history_large"]),
                },
            ),
            backups: Panel::new(
                now,
                Backups {
                    last_upload_at: Some(now.add_secs(-2.0 * 86_400.0)),
                    uploading: true,
                    status: Status::Degraded,
                    detail: None,
                },
            ),
            usage: Panel::new(
                now,
                Usage {
                    providers,
                    models_priced: Some(10_258),
                    // The fixture has no count of every model; the frame
                    // draws the catalog key where the count would go.
                    models_counted: None,
                    models_checked_at: Some(now.add_secs(-4.0 * 60.0)),
                },
            ),
            activity: Panel::new(now, activity),
            activity_log: Panel::new(now, activity_log(&v, now)),
            process_events: Panel::new(now, process_events),
            agents: Panel::new(now, agents),
            devices: Panel::new(now, devices),
            approvals: Panel::new(now, Approvals { waiting: Some(0) }),
            commands,
            log_view: None,
            last_result: None,
            serves: Serves::DESIGN,
            pairing: Vec::new(),
            previews: Vec::new(),
            applies: Vec::new(),
            daemon_connection: 0,
            upgrade: upgrade(&v, now),
        })
    }
}

#[cfg(all(test, feature = "fixture"))]
mod tests {
    use super::*;

    #[test]
    fn fixture_loads() {
        let s = Snapshot::fixture();
        assert_eq!(s.modules.data.len(), 25);
        assert_eq!(s.attention().len(), 2);
        assert_eq!(s.modules.data[3].id, "broca");
        assert_eq!(s.modules.data[3].cpu_history.len(), 60);
        assert_eq!(s.usage.data.providers.len(), 5);
        assert_eq!(s.agents.data.list.len(), 12);
        assert_eq!(s.commands.len(), 12);
    }

    #[test]
    fn shorter_load_history_is_a_prefix_of_the_longer() {
        // The fixture file records the build-server load twice, 40 samples
        // for the 120-column frame and 60 for the 160-column one. The model
        // keeps only the 60, which is right only if the 40 are its start.
        let v: Value = serde_json::from_str(FIXTURE_JSON).unwrap();
        let short: Vec<f64> = v["build_load_history_120"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap())
            .collect();
        let s = Snapshot::fixture();
        assert_eq!(short[..], s.build_server.data.load_history[..short.len()]);
    }
}
