//! What the screens show: a snapshot of the machine's state.
//!
//! Everything a screen displays that can change is a field here; renderers
//! hold only labels and headings. Each panel records when its data arrived,
//! so a renderer can tell fresh data from stale. Times are absolute
//! [`Timestamp`]s; renderers format them against a `now` and the host's UTC
//! offset.

#[cfg(feature = "fixture")]
use serde_json::Value;

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

/// One source's data and when it arrived.
#[derive(Clone, Debug, PartialEq)]
pub struct Panel<T> {
    pub received_at: Timestamp,
    pub data: T,
}

impl<T> Panel<T> {
    pub fn new(received_at: Timestamp, data: T) -> Self {
        Panel { received_at, data }
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    pub at: Timestamp,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub id: String,
    pub status: Status,
    /// When the module entered its current status, if known.
    pub status_since: Option<Timestamp>,
    pub pid: u32,
    pub started_at: Timestamp,
    pub restarts: u32,
    pub build: String,
    pub commit: String,
    pub cpu_percent: f64,
    pub memory_mb: f64,
    /// The module's own one-sentence health summary.
    pub summary: String,
    pub routes: u32,
    /// Why a restart is waiting, if one is.
    pub pending_restart: Option<String>,
    pub health_checked_at: Timestamp,
    pub metrics: Vec<Metric>,
    /// CPU percent samples, oldest first.
    pub cpu_history: Vec<f64>,
    /// Memory samples in MB, oldest first.
    pub memory_history: Vec<f64>,
    /// The tail of the module's log, oldest first.
    pub log: Vec<LogLine>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DaemonRunning {
    pub version: String,
    pub started_at: Timestamp,
    pub clients: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DaemonDown {
    pub stopped_at: Timestamp,
    /// Who stopped it and how, such as "by launchd, a normal shutdown".
    pub stopped_by: String,
    /// The daemon's last log line.
    pub last_line: String,
    /// The service manager's view, such as "loaded · restarts on failure".
    pub service: String,
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
    /// How the top bar names this machine, such as "This Mac".
    pub label: String,
    pub machine_id: String,
    pub utc_offset: UtcOffset,
    pub memory_used_gb: f64,
    pub memory_total_gb: f64,
    pub disk_used_gb: f64,
    pub disk_total_gb: f64,
    /// Load averages over 1, 5 and 15 minutes.
    pub load: [f64; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub struct BuildServer {
    pub busy: u32,
    pub slots: u32,
    pub queued: u32,
    pub wait_secs: f64,
    /// Load samples in percent, oldest first.
    pub load_history: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Backups {
    pub last_upload_at: Timestamp,
    pub uploading: bool,
    /// False when backups are behind.
    pub healthy: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    pub name: String,
    pub used: f64,
    pub resets_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    pub who: String,
    pub windows: Vec<UsageWindow>,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub name: String,
    pub window: String,
    /// Accounts' worth of capacity used, out of `of`.
    pub used: f64,
    pub of: u32,
    /// "account" or "accounts".
    pub unit: String,
    pub resets_at: Timestamp,
    pub accounts: Vec<Account>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub providers: Vec<Provider>,
    pub models_priced: u64,
    pub models_checked_at: Timestamp,
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

#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    pub name: String,
    pub project: String,
    /// "working", or how long it has been idle ("idle 4m").
    pub state: String,
    pub machine: String,
    pub doing: Option<String>,
}

impl Agent {
    pub fn working(&self) -> bool {
        self.state == "working"
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Agents {
    pub list: Vec<Agent>,
    pub working: u32,
    pub machines: u32,
    pub workspaces: u32,
    pub projects: u32,
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct Devices {
    pub list: Vec<Device>,
    pub online: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Approvals {
    pub waiting: u32,
}

/// An entry in the command palette and the area it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteCommand {
    pub title: String,
    pub area: String,
}

/// Everything the app shows, as of the times its panels record.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub host: Panel<Host>,
    pub daemon: Panel<Daemon>,
    pub modules: Panel<Vec<Module>>,
    pub build_server: Panel<BuildServer>,
    pub backups: Panel<Backups>,
    pub usage: Panel<Usage>,
    pub activity: Panel<Vec<ActivityEntry>>,
    pub process_events: Panel<Vec<ProcessEvent>>,
    pub agents: Panel<Agents>,
    pub devices: Panel<Devices>,
    pub approvals: Panel<Approvals>,
    pub commands: Vec<PaletteCommand>,
}

impl Snapshot {
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
                stopped_at: fixture::today(now, "09:29:51"),
                stopped_by: "by launchd, a normal shutdown".into(),
                last_line: "subc daemon stopping: SIGTERM".into(),
                service: "loaded · restarts on failure".into(),
            }),
        );
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
// that normal builds don't link the JSON parser or its exact-float setting.
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

    fn duration_from(now: Timestamp, text: &str) -> Option<Timestamp> {
        parse_duration(text).map(|secs| now.add_secs(secs))
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
                at: today(now, l[0].as_str().unwrap_or("00:00")),
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
                        at: today(now, clock),
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
                    pid: u(&m["pid"]),
                    started_at: now.add_secs(-f(&m["up"])),
                    restarts: u(&m["restarts"]),
                    build: s(&m["build"]),
                    commit: s(&m["commit"]),
                    cpu_percent: f(&m["cpu"]),
                    memory_mb: f(&m["mem"]),
                    summary: s(&m["summary"]),
                    routes: u(&m["routes"]),
                    pending_restart: m["pending"].as_str().map(str::to_string),
                    health_checked_at: now.add_secs(-1.0),
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
            label: "This Mac".into(),
            machine_id: "4f1a9c2e7b3d40c8a6e5d1b2c3f40912".into(),
            utc_offset: UtcOffset::UTC,
            memory_used_gb: f(&host["memUsed"]),
            memory_total_gb: f(&host["memTotal"]),
            disk_used_gb: f(&host["diskUsed"]),
            disk_total_gb: f(&host["diskTotal"]),
            load: [
                load.first().copied().unwrap_or(0.0),
                load.get(1).copied().unwrap_or(0.0),
                load.get(2).copied().unwrap_or(0.0),
            ],
        };

        let providers = arr(&v["quota"])
            .iter()
            .map(|q| Provider {
                name: s(&q["name"]),
                window: s(&q["window"]),
                used: f(&q["used"]),
                of: u(&q["of"]),
                unit: s(&q["unit"]),
                resets_at: duration_from(now, q["reset"].as_str().unwrap_or("")).unwrap_or(now),
                accounts: arr(&q["accounts"])
                    .iter()
                    .map(|a| Account {
                        who: s(&a["who"]),
                        note: s(&a["note"]),
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
        let agents = Agents {
            list: arr(&v["agents"])
                .iter()
                .map(|a| {
                    let name = s(&a[0]);
                    Agent {
                        project: s(&a[1]),
                        state: s(&a[2]),
                        machine: s(&a[3]),
                        doing: doing[name.as_str()]
                            .as_str()
                            .filter(|d| *d != "—")
                            .map(str::to_string),
                        name,
                    }
                })
                .collect(),
            working: 5,
            machines: 2,
            workspaces: 2,
            projects: 35,
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
                        name,
                    }
                })
                .collect(),
            online: 2,
        };

        let commands = arr(&v["commands"])
            .iter()
            .map(|c| PaletteCommand {
                title: s(&c[0]),
                area: s(&c[1]),
            })
            .collect();

        Ok(Snapshot {
            host: Panel::new(now, host),
            daemon: Panel::new(
                now,
                Daemon::Running(DaemonRunning {
                    version: "0.36.17".into(),
                    started_at: now.add_secs(-9.25 * 3600.0),
                    clients: 366,
                }),
            ),
            modules: Panel::new(now, modules),
            build_server: Panel::new(
                now,
                BuildServer {
                    busy: 80,
                    slots: 80,
                    queued: 21,
                    wait_secs: 180.0,
                    load_history: floats(&v["build_load_history_large"]),
                },
            ),
            backups: Panel::new(
                now,
                Backups {
                    last_upload_at: now.add_secs(-2.0 * 86_400.0),
                    uploading: true,
                    healthy: false,
                },
            ),
            usage: Panel::new(
                now,
                Usage {
                    providers,
                    models_priced: 10_258,
                    models_checked_at: now.add_secs(-4.0 * 60.0),
                },
            ),
            activity: Panel::new(now, activity),
            process_events: Panel::new(now, process_events),
            agents: Panel::new(now, agents),
            devices: Panel::new(now, devices),
            approvals: Panel::new(now, Approvals { waiting: 0 }),
            commands,
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
