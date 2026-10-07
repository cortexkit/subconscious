//! Health-checking an existing nats-server.
//!
//! `ck setup` does not install the message bus: nats-server and ck-bus are still placed
//! with ck-bus's own install tooling (`ck-bus install-plan` and `install-apply`, which
//! need the root signing ceremony). What setup does is upgrade an install it finds
//! already declared in `subc.jsonc`, so the daemon can health-check nats-server:
//!
//! 1. nats-server's `server.conf` gets the loopback monitoring listener
//!    (`http: "127.0.0.1:18222"`). The file is changed by ck-bus itself, through
//!    `ck-bus install-apply --conf-only --keep-existing`, so there is one renderer and
//!    a port the operator already chose is kept rather than overwritten.
//! 2. Only after that succeeds, `modules.nats-server.health` gets
//!    `{ http: <the listener's /healthz>, cadence_ms: 30000, deadline_ms: 5000 }`,
//!    unless a `health` entry already exists, which is the operator's and is kept.
//!
//! The order matters: a health check enabled before nats-server has its listener would
//! fail every probe against a healthy server. A running nats-server opens the listener
//! only when it restarts, and the daemon applies the health entry on its next rescan,
//! so setup tells the operator to restart nats-server and then rescan. Setup itself
//! restarts neither.
//!
//! An install is recognised only when nats-server is declared with `protocol: "none"`
//! and `-c <absolute path>/server.conf`, and ck-bus is declared with an absolute
//! `program`. Anything else is reported and skipped; setup never guesses a path.

use std::{
    fmt,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{json, Value};

use super::config;

pub const NATS_SERVER_MODULE: &str = "nats-server";
pub const CKBUS_MODULE: &str = "ckbus";
const SERVER_CONF_FILE: &str = "server.conf";
const HEALTH_KEY: &str = "modules.nats-server.health";
const HEALTH_CADENCE_MS: u64 = 30_000;
const HEALTH_DEADLINE_MS: u64 = 5_000;
/// The only host the monitoring listener may be on. ck-bus refuses any other host in
/// `server.conf`; setup checks the URL ck-bus reports again before writing it.
const LOOPBACK_HOST: &str = "127.0.0.1";

/// The declared install setup would upgrade.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BusTarget {
    /// ck-bus's executable, from `modules.ckbus.program`.
    pub ckbus: PathBuf,
    /// The directory holding nats-server's `server.conf`.
    pub nats_dir: PathBuf,
}

/// What `ck-bus install-apply --conf-only` reported about the listener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListenerState {
    /// The listener is absent; a dry run reports it would be added.
    WouldAdd,
    /// The listener was just added.
    Added,
    /// The listener is already on the default port.
    Present,
    /// The listener is on a port the operator chose, which was kept.
    Kept,
}

/// Whether `modules.nats-server.health` needs writing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HealthState {
    Missing,
    /// The entry setup would write is already there.
    Matching,
    /// A `health` entry the operator wrote that differs from setup's; setup never
    /// overwrites it.
    Kept,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BusObserved {
    pub target: BusTarget,
    pub listener: ListenerState,
    pub health_url: String,
    pub health: HealthState,
}

impl BusObserved {
    pub fn needs_change(&self) -> bool {
        self.listener == ListenerState::WouldAdd || self.health == HealthState::Missing
    }

    /// The verbose-output line for an install whose monitoring listener and health
    /// check are both already in place.
    pub fn noop_scope(&self) -> String {
        let mut scope =
            "nats-server monitoring listener and health check are already configured".to_string();
        if self.listener == ListenerState::Kept {
            scope.push_str(&format!(
                "; kept your monitoring listener ({})",
                self.health_url
            ));
        }
        if self.health == HealthState::Kept {
            scope.push_str(&format!("; kept your setting {HEALTH_KEY}"));
        }
        scope
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum BusMonitoring {
    /// No nats-server is declared; setup has nothing to upgrade.
    #[default]
    NotDeclared,
    /// nats-server is declared but setup cannot upgrade it safely.
    Skipped {
        reason: String,
    },
    Observed(BusObserved),
}

impl fmt::Display for BusMonitoring {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotDeclared => formatter.write_str("no nats-server declared"),
            Self::Skipped { reason } => write!(
                formatter,
                "nats-server health check skipped: {reason}; nothing was changed"
            ),
            Self::Observed(observed) => formatter.write_str(&observed.noop_scope()),
        }
    }
}

/// What `ck-bus install-apply --conf-only` printed: whether it added, kept or found the
/// monitoring listener (`status`), and the `/healthz` URL that listener serves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfReport {
    pub status: String,
    pub health_url: String,
}

/// Runs `install-apply --conf-only --keep-existing` for one install. Tests replace
/// the process with a fake so they never run an installed ck-bus.
pub trait ConfTool {
    fn conf_only(&mut self, target: &BusTarget, dry_run: bool) -> Result<ConfReport, String>;
}

/// The real ck-bus, run as the program the daemon configuration names.
pub struct CkBusCommand;

impl ConfTool for CkBusCommand {
    fn conf_only(&mut self, target: &BusTarget, dry_run: bool) -> Result<ConfReport, String> {
        let mut command = Command::new(&target.ckbus);
        command
            .args([
                "install-apply",
                "--conf-only",
                "--keep-existing",
                "--nats-dir",
            ])
            .arg(&target.nats_dir);
        if dry_run {
            command.arg("--dry-run");
        }
        // ck-bus handles install commands before it reads SUBC_MODULE_ID, so this run
        // never acts as the supervised module. Removing the variable keeps it that way
        // if setup itself was started from a supervised module's environment.
        command.env_remove("SUBC_MODULE_ID");
        let output = command.output().map_err(|error| {
            format!(
                "could not run {} install-apply --conf-only: {error}",
                target.ckbus.display()
            )
        })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if detail.is_empty() {
                format!(
                    "ck-bus install-apply --conf-only failed with {}",
                    output.status
                )
            } else {
                detail
            });
        }
        parse_report(&output.stdout)
    }
}

fn parse_report(stdout: &[u8]) -> Result<ConfReport, String> {
    let value: Value = serde_json::from_slice(stdout).map_err(|error| {
        format!("ck-bus install-apply --conf-only printed invalid JSON: {error}")
    })?;
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| format!("ck-bus install-apply --conf-only omitted {name}"))
    };
    Ok(ConfReport {
        status: field("status")?,
        health_url: field("health_url")?,
    })
}

/// Refuses a health URL that is not `http://127.0.0.1:<port>/healthz`. ck-bus already
/// refuses a non-loopback listener; this keeps setup from writing anything else even
/// if a different ck-bus reported it.
fn check_loopback_health_url(url: &str) -> Result<(), String> {
    let port = url
        .strip_prefix(&format!("http://{LOOPBACK_HOST}:"))
        .and_then(|rest| rest.strip_suffix("/healthz"))
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0);
    match port {
        Some(_) => Ok(()),
        None => Err(format!(
            "refusing nats-server monitoring address {url}: the listener must be on {LOOPBACK_HOST}"
        )),
    }
}

fn listener_state(status: &str, dry_run: bool) -> Result<ListenerState, String> {
    match (status, dry_run) {
        ("would apply", true) => Ok(ListenerState::WouldAdd),
        ("applied", false) => Ok(ListenerState::Added),
        ("unchanged", _) => Ok(ListenerState::Present),
        ("kept", _) => Ok(ListenerState::Kept),
        _ => Err(format!(
            "ck-bus install-apply --conf-only reported an unexpected status {status:?}"
        )),
    }
}

fn desired_health(health_url: &str) -> Value {
    json!({
        "http": health_url,
        "cadence_ms": HEALTH_CADENCE_MS,
        "deadline_ms": HEALTH_DEADLINE_MS,
    })
}

fn health_state(document: &Value, health_url: &str) -> HealthState {
    match config::existing_value(document, HEALTH_KEY) {
        None => HealthState::Missing,
        Some(existing) if *existing == desired_health(health_url) => HealthState::Matching,
        Some(_) => HealthState::Kept,
    }
}

/// Finds the declared install, or why it cannot be upgraded.
fn declared_target(document: &Value) -> Result<Option<BusTarget>, String> {
    let Some(nats) = config::existing_value(document, &format!("modules.{NATS_SERVER_MODULE}"))
    else {
        return Ok(None);
    };
    if nats.get("protocol").and_then(Value::as_str) != Some("none") {
        return Err(format!(
            "modules.{NATS_SERVER_MODULE} is not declared with protocol \"none\", and a health.http check is valid only for that protocol"
        ));
    }
    let args = nats
        .get("args")
        .and_then(Value::as_array)
        .map(|args| args.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut confs = args
        .windows(2)
        .filter(|pair| matches!(pair[0], "-c" | "--config"))
        .map(|pair| pair[1]);
    let conf = match (confs.next(), confs.next()) {
        (Some(conf), None) => Path::new(conf),
        (None, _) => {
            return Err(format!(
                "modules.{NATS_SERVER_MODULE}.args names no `-c <server.conf>`"
            ))
        }
        (Some(_), Some(_)) => {
            return Err(format!(
                "modules.{NATS_SERVER_MODULE}.args names more than one configuration file"
            ))
        }
    };
    if !conf.is_absolute() {
        return Err(format!(
            "nats-server's configuration path {} is not absolute",
            conf.display()
        ));
    }
    if conf.file_name().and_then(|name| name.to_str()) != Some(SERVER_CONF_FILE) {
        return Err(format!(
            "nats-server's configuration {} is not a {SERVER_CONF_FILE} written by ck-bus install-apply",
            conf.display()
        ));
    }
    let nats_dir = conf
        .parent()
        .ok_or_else(|| format!("{} has no directory", conf.display()))?;
    let ckbus = config::existing_value(document, &format!("modules.{CKBUS_MODULE}.program"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| format!("modules.{CKBUS_MODULE}.program is not declared"))?;
    if !ckbus.is_absolute() {
        return Err(format!(
            "ck-bus's program {} is not an absolute path",
            ckbus.display()
        ));
    }
    if !ckbus.is_file() {
        return Err(format!(
            "ck-bus's program {} is not a file",
            ckbus.display()
        ));
    }
    Ok(Some(BusTarget {
        ckbus,
        nats_dir: nats_dir.to_path_buf(),
    }))
}

/// Observes an existing install without changing anything: ck-bus runs with
/// `--dry-run`, and the configuration is only read.
pub fn observe(config_path: &Path, tool: &mut impl ConfTool) -> BusMonitoring {
    if !config_path.is_file() {
        return BusMonitoring::NotDeclared;
    }
    let document = match config::read_document(config_path) {
        Ok((_, document)) => document,
        // An unreadable configuration is reported by the components that need it.
        Err(_) => return BusMonitoring::NotDeclared,
    };
    let target = match declared_target(&document) {
        Ok(Some(target)) => target,
        Ok(None) => return BusMonitoring::NotDeclared,
        Err(reason) => return BusMonitoring::Skipped { reason },
    };
    let report = match tool.conf_only(&target, true) {
        Ok(report) => report,
        Err(error) => {
            return BusMonitoring::Skipped {
                reason: format!("ck-bus refused to add the monitoring listener: {error}"),
            }
        }
    };
    let observed = listener_state(&report.status, true)
        .and_then(|listener| check_loopback_health_url(&report.health_url).map(|()| listener));
    match observed {
        Ok(listener) => BusMonitoring::Observed(BusObserved {
            health: health_state(&document, &report.health_url),
            target,
            listener,
            health_url: report.health_url,
        }),
        Err(reason) => BusMonitoring::Skipped { reason },
    }
}

/// Adds the listener, then the health entry, and returns what was done as lines for
/// the operator. Nothing is written to `subc.jsonc` unless ck-bus succeeded and
/// reported a loopback listener.
pub fn apply(
    config_path: &Path,
    target: &BusTarget,
    tool: &mut impl ConfTool,
) -> Result<Vec<String>, String> {
    let conf = target.nats_dir.join(SERVER_CONF_FILE);
    let report = tool.conf_only(target, false)?;
    let listener = listener_state(&report.status, false)?;
    check_loopback_health_url(&report.health_url)?;

    let mut lines = vec![match listener {
        ListenerState::Added => format!(
            "added nats-server's monitoring listener ({}) to {}",
            report.health_url,
            conf.display()
        ),
        ListenerState::Kept => format!(
            "kept your setting: nats-server's monitoring listener in {} stays as you set it; the health check uses {}",
            conf.display(),
            report.health_url
        ),
        ListenerState::Present | ListenerState::WouldAdd => format!(
            "nats-server's monitoring listener ({}) is already in {}",
            report.health_url,
            conf.display()
        ),
    }];

    let health_added = match config::plan_missing_value(
        config_path,
        HEALTH_KEY,
        desired_health(&report.health_url),
    )? {
        Some(change) => {
            config::apply(&change)?;
            lines.push(format!(
                "added the nats-server health check ({}) to {}",
                report.health_url,
                config_path.display()
            ));
            true
        }
        None => {
            let (_, document) = config::read_document(config_path)?;
            if health_state(&document, &report.health_url) == HealthState::Kept {
                lines.push(format!(
                    "kept your setting: {HEALTH_KEY} in {} is left as it is",
                    config_path.display()
                ));
            }
            false
        }
    };

    // A running nats-server opens a newly added listener only when it restarts. When
    // the listener was already in the file, only the daemon has to pick up the new
    // health entry, so a restart would be needless.
    if listener == ListenerState::Added {
        lines.push(
            "to start the health check: restart nats-server so it opens the listener \
             (`ck module restart nats-server`), then run `ck module rescan`"
                .to_string(),
        );
    } else if health_added {
        lines.push("to start the health check: run `ck module rescan`".to_string());
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_test_support::TestTempDir;

    /// A stand-in for ck-bus. It follows `install-apply --conf-only --keep-existing`
    /// for the files these tests write: a `server.conf` without an `http:` line gets the
    /// default listener, and an existing one is kept. It records `subc.jsonc` at the
    /// moment the listener is written, which is what the ordering test reads.
    struct FakeCkBus {
        config_path: PathBuf,
        calls: Vec<bool>,
        config_at_write: Option<String>,
        refuse: Option<String>,
    }

    impl FakeCkBus {
        fn new(config_path: &Path) -> Self {
            Self {
                config_path: config_path.to_path_buf(),
                calls: Vec::new(),
                config_at_write: None,
                refuse: None,
            }
        }
    }

    impl ConfTool for FakeCkBus {
        fn conf_only(&mut self, target: &BusTarget, dry_run: bool) -> Result<ConfReport, String> {
            self.calls.push(dry_run);
            if let Some(reason) = &self.refuse {
                return Err(reason.clone());
            }
            let path = target.nats_dir.join(SERVER_CONF_FILE);
            let conf = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
            let existing = conf.lines().find_map(|line| {
                line.strip_prefix("http: \"127.0.0.1:")
                    .and_then(|rest| rest.strip_suffix('"'))
                    .map(ToOwned::to_owned)
            });
            let (status, port) = match existing {
                Some(port) if port == "18222" => ("unchanged", port),
                Some(port) => ("kept", port),
                None if dry_run => ("would apply", "18222".to_string()),
                None => {
                    self.config_at_write = std::fs::read_to_string(&self.config_path).ok();
                    std::fs::write(&path, format!("{conf}http: \"127.0.0.1:18222\"\n"))
                        .map_err(|error| error.to_string())?;
                    ("applied", "18222".to_string())
                }
            };
            Ok(ConfReport {
                status: status.to_string(),
                health_url: format!("http://127.0.0.1:{port}/healthz"),
            })
        }
    }

    struct Install {
        _dir: TestTempDir,
        config_path: PathBuf,
        conf_path: PathBuf,
        ckbus: PathBuf,
    }

    /// A hand-placed bus in a temporary directory: `server.conf` (with `http` when
    /// given) and a `subc.jsonc` declaring nats-server and ck-bus, plus `extra`
    /// members inside the nats-server declaration.
    fn install(http: Option<&str>, extra: &str) -> Install {
        let dir = TestTempDir::new("bus-monitoring");
        let nats = dir.join("nats");
        std::fs::create_dir_all(&nats).unwrap();
        let conf_path = nats.join(SERVER_CONF_FILE);
        let mut conf = "listen: \"127.0.0.1:14222\"\n".to_string();
        if let Some(http) = http {
            conf.push_str(&format!("http: \"{http}\"\n"));
        }
        std::fs::write(&conf_path, conf).unwrap();
        let ckbus = dir.join("ck-bus");
        std::fs::write(&ckbus, "not executed by these tests").unwrap();
        let config_path = dir.join("subc.jsonc");
        std::fs::write(
            &config_path,
            format!(
                "{{\n  // the operator's comment survives\n  \"version\": 1,\n  \"modules\": {{\n    \"ckbus\": {{ \"program\": {ckbus} }},\n    \"nats-server\": {{\n      \"program\": \"/opt/homebrew/bin/nats-server\",\n      \"args\": [\"-c\", {conf}],\n      \"protocol\": \"none\"{extra}\n    }}\n  }}\n}}\n",
                ckbus = serde_json::to_string(&ckbus).unwrap(),
                conf = serde_json::to_string(&conf_path).unwrap(),
            ),
        )
        .unwrap();
        Install {
            _dir: dir,
            config_path,
            conf_path,
            ckbus,
        }
    }

    fn health(install: &Install) -> Option<Value> {
        let (_, document) = config::read_document(&install.config_path).unwrap();
        config::existing_value(&document, HEALTH_KEY).cloned()
    }

    fn observed(monitoring: BusMonitoring) -> BusObserved {
        match monitoring {
            BusMonitoring::Observed(observed) => observed,
            other => panic!("expected an observed install, got {other:?}"),
        }
    }

    #[test]
    fn first_run_over_an_unmonitored_install_writes_the_listener_and_the_health_check() {
        let install = install(None, "");
        let mut tool = FakeCkBus::new(&install.config_path);
        let bus = observed(observe(&install.config_path, &mut tool));
        assert_eq!(bus.listener, ListenerState::WouldAdd);
        assert_eq!(bus.health, HealthState::Missing);
        assert!(bus.needs_change());
        assert_eq!(
            bus.target,
            BusTarget {
                ckbus: install.ckbus.clone(),
                nats_dir: install.conf_path.parent().unwrap().to_path_buf(),
            }
        );
        assert!(
            !std::fs::read_to_string(&install.conf_path)
                .unwrap()
                .contains("http:"),
            "observing writes nothing"
        );

        let lines = apply(&install.config_path, &bus.target, &mut tool).unwrap();
        assert!(std::fs::read_to_string(&install.conf_path)
            .unwrap()
            .contains("http: \"127.0.0.1:18222\"\n"));
        assert_eq!(
            health(&install),
            Some(json!({
                "http": "http://127.0.0.1:18222/healthz",
                "cadence_ms": 30000,
                "deadline_ms": 5000,
            }))
        );
        let config = std::fs::read_to_string(&install.config_path).unwrap();
        assert!(
            config.contains("// the operator's comment survives"),
            "{config}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("ck module restart nats-server")),
            "{lines:?}"
        );
        // The daemon's own parser accepts what was written.
        subc_daemon::daemon_config::load(&install.config_path)
            .expect("the daemon loads the written configuration")
            .expect("the configuration exists");
    }

    #[test]
    fn a_second_run_changes_nothing() {
        let install = install(None, "");
        let mut tool = FakeCkBus::new(&install.config_path);
        let bus = observed(observe(&install.config_path, &mut tool));
        apply(&install.config_path, &bus.target, &mut tool).unwrap();
        let config = std::fs::read(&install.config_path).unwrap();
        let conf = std::fs::read(&install.conf_path).unwrap();

        let again = observed(observe(&install.config_path, &mut tool));
        assert_eq!(again.listener, ListenerState::Present);
        assert_eq!(again.health, HealthState::Matching);
        assert!(!again.needs_change(), "a re-run plans no operation");
        // Even if the operation were run again, it writes nothing new.
        let lines = apply(&install.config_path, &again.target, &mut tool).unwrap();
        assert_eq!(std::fs::read(&install.config_path).unwrap(), config);
        assert_eq!(std::fs::read(&install.conf_path).unwrap(), conf);
        assert!(
            !lines.iter().any(|line| line.contains("restart")),
            "{lines:?}"
        );
    }

    #[test]
    fn an_operator_s_health_entry_and_listener_port_are_kept() {
        let user_health = r#",
      "health": { "http": "http://127.0.0.1:19222/healthz", "cadence_ms": 1000, "on_failing": "restart" }"#;
        let install = install(Some("127.0.0.1:19222"), user_health);
        let config = std::fs::read(&install.config_path).unwrap();
        let conf = std::fs::read(&install.conf_path).unwrap();
        let mut tool = FakeCkBus::new(&install.config_path);
        let bus = observed(observe(&install.config_path, &mut tool));
        assert_eq!(bus.listener, ListenerState::Kept);
        assert_eq!(bus.health, HealthState::Kept);
        assert!(!bus.needs_change());
        assert!(bus.noop_scope().contains("kept your setting"));

        let lines = apply(&install.config_path, &bus.target, &mut tool).unwrap();
        assert_eq!(std::fs::read(&install.config_path).unwrap(), config);
        assert_eq!(std::fs::read(&install.conf_path).unwrap(), conf);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("kept your setting"))
                .count(),
            2,
            "{lines:?}"
        );
    }

    #[test]
    fn an_operator_s_health_entry_is_kept_while_the_missing_listener_is_added() {
        let user_health = r#",
      "health": { "on_failing": "report" }"#;
        let install = install(None, user_health);
        let config = std::fs::read(&install.config_path).unwrap();
        let mut tool = FakeCkBus::new(&install.config_path);
        let bus = observed(observe(&install.config_path, &mut tool));
        assert_eq!(bus.health, HealthState::Kept);
        assert!(bus.needs_change(), "the listener is still missing");
        apply(&install.config_path, &bus.target, &mut tool).unwrap();
        assert!(std::fs::read_to_string(&install.conf_path)
            .unwrap()
            .contains("http: \"127.0.0.1:18222\"\n"));
        assert_eq!(std::fs::read(&install.config_path).unwrap(), config);
    }

    #[test]
    fn a_kept_listener_port_is_the_one_the_new_health_check_probes() {
        let install = install(Some("127.0.0.1:19222"), "");
        let mut tool = FakeCkBus::new(&install.config_path);
        let bus = observed(observe(&install.config_path, &mut tool));
        assert_eq!(bus.listener, ListenerState::Kept);
        apply(&install.config_path, &bus.target, &mut tool).unwrap();
        assert_eq!(
            health(&install).unwrap()["http"],
            "http://127.0.0.1:19222/healthz"
        );
    }

    #[test]
    fn a_listener_already_in_the_file_asks_only_for_a_rescan() {
        // nats-server already serves this listener, so restarting it would only
        // interrupt the bus; the daemon picks up the new health entry on rescan.
        let install = install(Some("127.0.0.1:19222"), "");
        let mut tool = FakeCkBus::new(&install.config_path);
        let bus = observed(observe(&install.config_path, &mut tool));
        let lines = apply(&install.config_path, &bus.target, &mut tool).unwrap();
        assert!(health(&install).is_some(), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("run `ck module rescan`")),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("ck module restart nats-server")),
            "{lines:?}"
        );
    }

    #[test]
    fn the_listener_is_written_before_the_health_check() {
        let install = install(None, "");
        let mut tool = FakeCkBus::new(&install.config_path);
        let bus = observed(observe(&install.config_path, &mut tool));
        apply(&install.config_path, &bus.target, &mut tool).unwrap();
        let at_write = tool
            .config_at_write
            .as_deref()
            .expect("the listener was written");
        assert!(
            !at_write.contains("\"health\""),
            "subc.jsonc already had the health check when the listener was written:\n{at_write}"
        );
        assert!(health(&install).is_some());
        assert_eq!(tool.calls, [true, false], "observe dry-runs, apply writes");
    }

    #[test]
    fn a_refused_listener_writes_no_health_check() {
        let install = install(None, "");
        let config = std::fs::read(&install.config_path).unwrap();
        let mut tool = FakeCkBus::new(&install.config_path);
        tool.refuse = Some("server.conf: not an install-apply rendered file".to_string());
        match observe(&install.config_path, &mut tool) {
            BusMonitoring::Skipped { reason } => {
                assert!(reason.contains("not an install-apply"), "{reason}")
            }
            other => panic!("expected a skip, got {other:?}"),
        }
        let target = BusTarget {
            ckbus: install.ckbus.clone(),
            nats_dir: install.conf_path.parent().unwrap().to_path_buf(),
        };
        assert!(apply(&install.config_path, &target, &mut tool).is_err());
        assert_eq!(std::fs::read(&install.config_path).unwrap(), config);
    }

    /// A ck-bus reporting a listener on another host is refused before anything is
    /// written to `subc.jsonc`.
    #[test]
    fn a_non_loopback_listener_is_refused() {
        struct Exposed;
        impl ConfTool for Exposed {
            fn conf_only(&mut self, _: &BusTarget, dry_run: bool) -> Result<ConfReport, String> {
                Ok(ConfReport {
                    status: if dry_run { "would apply" } else { "applied" }.to_string(),
                    health_url: "http://0.0.0.0:18222/healthz".to_string(),
                })
            }
        }
        let install = install(None, "");
        let config = std::fs::read(&install.config_path).unwrap();
        assert!(matches!(
            observe(&install.config_path, &mut Exposed),
            BusMonitoring::Skipped { .. }
        ));
        let target = BusTarget {
            ckbus: install.ckbus.clone(),
            nats_dir: install.conf_path.parent().unwrap().to_path_buf(),
        };
        let error = apply(&install.config_path, &target, &mut Exposed).unwrap_err();
        assert!(error.contains("must be on 127.0.0.1"), "{error}");
        assert_eq!(std::fs::read(&install.config_path).unwrap(), config);
        for url in [
            "http://localhost:18222/healthz",
            "http://[::1]:18222/healthz",
            "http://127.0.0.1:0/healthz",
            "http://127.0.0.1:18222/healthz?js-enabled-only=true",
        ] {
            assert!(check_loopback_health_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn an_install_setup_cannot_identify_is_skipped_not_guessed() {
        let mut tool = FakeCkBus::new(Path::new("/nonexistent"));
        let dir = TestTempDir::new("bus-monitoring-skips");
        let config_path = dir.join("subc.jsonc");
        // Absolute on every platform, so each case fails for the reason it names.
        let server_conf = json!(dir.join("n").join("server.conf"));
        let other_conf = json!(dir.join("n").join("other.conf"));
        let nats = |args: Value, protocol: Option<&str>| {
            let mut entry = json!({ "program": "nats-server", "args": args });
            if let Some(protocol) = protocol {
                entry["protocol"] = json!(protocol);
            }
            entry
        };
        let cases = [
            (json!({ "modules": { "aft": { "program": "x" } } }), None),
            (
                json!({ "modules": { "nats-server": nats(json!(["-c", server_conf]), None) } }),
                Some("protocol"),
            ),
            (
                json!({ "modules": { "nats-server": nats(json!(["-c", server_conf]), Some("subc")) } }),
                Some("protocol"),
            ),
            (
                json!({ "modules": { "nats-server": nats(json!([]), Some("none")) } }),
                Some("-c <server.conf>"),
            ),
            (
                json!({ "modules": { "nats-server": nats(json!(["-c", server_conf, "--config", server_conf]), Some("none")) } }),
                Some("more than one"),
            ),
            (
                json!({ "modules": { "nats-server": nats(json!(["-c", "nats/server.conf"]), Some("none")) } }),
                Some("not absolute"),
            ),
            (
                json!({ "modules": { "nats-server": nats(json!(["-c", other_conf]), Some("none")) } }),
                Some("not a server.conf"),
            ),
            (
                json!({ "modules": { "nats-server": nats(json!(["-c", server_conf]), Some("none")) } }),
                Some("modules.ckbus.program"),
            ),
            (
                json!({ "modules": {
                    "ckbus": { "program": "ck-bus" },
                    "nats-server": nats(json!(["-c", server_conf]), Some("none")),
                } }),
                Some("not an absolute path"),
            ),
            (
                json!({ "modules": {
                    "ckbus": { "program": dir.join("absent-ck-bus") },
                    "nats-server": nats(json!(["-c", server_conf]), Some("none")),
                } }),
                Some("is not a file"),
            ),
        ];
        for (config, expected) in cases {
            let config = config.to_string();
            std::fs::write(&config_path, &config).unwrap();
            match (observe(&config_path, &mut tool), expected) {
                (BusMonitoring::NotDeclared, None) => {}
                (BusMonitoring::Skipped { reason }, Some(expected)) => {
                    assert!(reason.contains(expected), "{reason} lacks {expected}")
                }
                (other, _) => panic!("{config}: unexpected {other:?}"),
            }
        }
        assert!(
            tool.calls.is_empty(),
            "ck-bus never runs for a skipped install"
        );
    }

    /// The real command line, against a stand-in program in a temporary directory.
    #[cfg(unix)]
    #[test]
    fn the_ck_bus_command_line_is_conf_only_and_keeps_existing_settings() {
        let dir = TestTempDir::new("bus-monitoring-command");
        let program = dir.join("ck-bus");
        super::super::test_exec::write_executable(
            &program,
            b"#!/bin/sh\nif [ -n \"$SUBC_MODULE_ID\" ]; then echo 'module id leaked' >&2; exit 1; fi\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/argv\"\nprintf '{\"status\":\"would apply\",\"server_conf\":\"x\",\"health_url\":\"http://127.0.0.1:18222/healthz\"}'\n",
        );
        let target = BusTarget {
            ckbus: program,
            nats_dir: dir.join("nats"),
        };
        let report = CkBusCommand.conf_only(&target, true).unwrap();
        assert_eq!(report.status, "would apply");
        assert_eq!(report.health_url, "http://127.0.0.1:18222/healthz");
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            [
                "install-apply",
                "--conf-only",
                "--keep-existing",
                "--nats-dir",
                dir.join("nats").to_str().unwrap(),
                "--dry-run",
            ]
        );
    }
}
