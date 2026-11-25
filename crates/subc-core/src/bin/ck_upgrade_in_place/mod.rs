use std::{
    ffi::OsString,
    fs::File,
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subc_control::{upgrade_in_place, ClientControlRequest};
use subc_protocol::{ErrorBody, FrameType};
use tokio::time;

use super::{parse_daemon_lint, CkClient, CkError, Command, JsonOutput, ResolvedConnection};

pub(super) const DAEMON_HELP: &str = "ck daemon — inspect or upgrade the running daemon\n\nusage:\n  ck [--json] [--verbose] daemon\n  ck [--json] daemon upgrade --in-place\n  ck [--json] daemon triage\n  ck daemon lint [<config>] [--verbose]\n\n  upgrade --in-place replaces the daemon with the executable at its startup path (Linux and macOS).\n  triage reads only the local run directory; it never contacts the daemon.\n  lint reads module manifests without connecting to the daemon.";

pub(super) fn parse_daemon(tail: &[OsString]) -> Result<Command, CkError> {
    if tail
        .iter()
        .any(|arg| arg == "-h" || arg == "--help" || arg == "help")
    {
        return Ok(Command::Help(DAEMON_HELP.into()));
    }
    if tail.first().is_some_and(|verb| verb == "lint") {
        return parse_daemon_lint(&tail[1..]);
    }
    let positional = tail
        .iter()
        .filter(|arg| *arg != "--verbose")
        .collect::<Vec<_>>();
    match positional.as_slice() {
        [] => Ok(Command::Daemon),
        [verb] if *verb == "announce-restart" => Ok(Command::DaemonAnnounceRestart),
        [verb] if *verb == "triage" => Ok(Command::DaemonTriage),
        [verb, flag] if *verb == "upgrade" && *flag == "--in-place" => {
            Ok(Command::DaemonUpgradeInPlace)
        }
        [verb, ..] if *verb == "upgrade" => Err(CkError::Usage(
            "usage: ck [--json] daemon upgrade --in-place".into(),
        )),
        _ => Ok(Command::Help(DAEMON_HELP.into())),
    }
}

const REPLY_WAIT: Duration = Duration::from_secs(120);
const POLL_WAIT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Deserialize, Serialize, PartialEq)]
struct Build {
    core_version: String,
    build_git_sha: Option<String>,
}

#[derive(Deserialize)]
struct Identity {
    pid: u32,
    executable: PathBuf,
    executable_sha256: Option<String>,
    #[serde(flatten)]
    build: Build,
}

#[derive(Deserialize)]
struct Receipt {
    outcome: String,
    attempt_id: String,
    expected: Build,
}

#[derive(Deserialize)]
struct Attempt {
    attempt_id: String,
    started_at_ms: u64,
    outcome: String,
    stage: Option<String>,
    #[serde(default)]
    restarted: Vec<String>,
    modules: Option<Vec<ModuleDisposition>>,
}

#[derive(Deserialize)]
struct ModuleDisposition {
    id: String,
    disposition: String,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Upgraded,
    Refused,
    Failed,
    Unknown,
}

#[derive(Serialize)]
struct Report {
    outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt_id: Option<String>,
    daemon_pid: u32,
    carried: Vec<String>,
    restarted: Vec<String>,
    stopped: Vec<String>,
    blocked: Vec<String>,
    build: Option<Build>,
}

impl Report {
    fn new(daemon_pid: u32, build: Option<Build>) -> Self {
        Self {
            outcome: Outcome::Unknown,
            reason: None,
            attempt_id: None,
            daemon_pid,
            carried: vec![],
            restarted: vec![],
            stopped: vec![],
            blocked: vec![],
            build,
        }
    }

    fn refuse(mut self, code: &str) -> Self {
        self.outcome = Outcome::Refused;
        self.reason = Some(code.to_owned());
        self
    }

    fn record(&mut self, identity: Identity, attempt: &Attempt) {
        self.build = Some(identity.build);
        self.attempt_id = Some(attempt.attempt_id.clone());
        self.carried.clear();
        self.restarted.clone_from(&attempt.restarted);
        self.stopped.clear();
        self.blocked.clear();
        for module in attempt.modules.iter().flatten() {
            let group = match module.disposition.as_str() {
                "resumed" => &mut self.carried,
                "restarted" => &mut self.restarted,
                "stopped" => &mut self.stopped,
                "blocked" => &mut self.blocked,
                _ => continue,
            };
            group.push(module.id.clone());
        }
    }

    fn print(self, json: JsonOutput) -> Result<(), CkError> {
        let (label, exit_code) = match self.outcome {
            Outcome::Upgraded => ("upgraded", 0),
            Outcome::Refused => ("refused before any change", 3),
            Outcome::Failed => ("failed", 4),
            Outcome::Unknown => ("unknown", 5),
        };
        if json.enabled {
            json.print(&serde_json::to_value(&self)?)?;
        } else {
            println!("daemon upgrade: {label}");
            if let Some(reason) = self.reason {
                println!("reason: {reason}");
            }
            if let Some(attempt) = self.attempt_id {
                println!("attempt: {attempt}");
            }
            for (name, ids) in [
                ("carried", self.carried),
                ("restarted", self.restarted),
                ("stopped", self.stopped),
                ("blocked", self.blocked),
            ] {
                if !ids.is_empty() {
                    println!("{name}: {}", ids.join(", "));
                }
            }
        }
        if exit_code == 0 {
            Ok(())
        } else {
            Err(CkError::DaemonUpgradeExit { exit_code })
        }
    }
}

fn read<T: serde::de::DeserializeOwned>(value: &Value) -> Option<T> {
    T::deserialize(value).ok()
}

fn checksum(path: &Path) -> io::Result<String> {
    let mut digest = Sha256::new();
    io::copy(&mut File::open(path)?, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) async fn run(mut client: CkClient, json: JsonOutput) -> Result<(), CkError> {
    let described = client
        .rpc_value(ClientControlRequest::ServerDescribe {})
        .await?;
    let counters = &described["counters"];
    let Some(identity) = read::<Identity>(&counters["daemon_identity"]) else {
        return Report::new(client.info.pid, None)
            .refuse(upgrade_in_place::UPGRADE_IN_PLACE_UNSUPPORTED)
            .print(json);
    };
    let prior = counters["last_upgrade"]["attempt_id"].as_str();
    let mut report = Report::new(identity.pid, Some(identity.build));
    let Ok(executable_sha256) = checksum(&identity.executable) else {
        return report
            .refuse(upgrade_in_place::UPGRADE_EXECUTABLE_MISMATCH)
            .print(json);
    };

    let called_at_ms = unix_ms();
    let reply = client
        .rpc_frame_with_timeout(
            ClientControlRequest::DaemonUpgradeInPlace {
                executable_sha256: executable_sha256.clone(),
            },
            REPLY_WAIT,
        )
        .await
        .ok();
    if let Some(error) = reply
        .as_ref()
        .filter(|f| f.header.ty == FrameType::Error)
        .and_then(|frame| serde_json::from_slice::<ErrorBody>(&frame.body).ok())
    {
        if error.code != upgrade_in_place::UPGRADE_FAILED {
            return report.refuse(&error.code).print(json);
        }
        report.outcome = Outcome::Failed;
        let detail = error.detail.unwrap_or_default();
        report.reason = read(&detail["stage"]).or(Some(error.code));
        report.attempt_id = read(&detail["attempt_id"]);
        report.restarted = read(&detail["restarted"]).unwrap_or_default();
        return report.print(json);
    }
    let receipt = reply
        .filter(|f| f.header.ty == FrameType::Response)
        .and_then(|frame| serde_json::from_slice::<Receipt>(&frame.body).ok())
        .filter(|receipt| receipt.outcome == "exec_started");
    let expected = receipt.map(|receipt| {
        report.attempt_id = Some(receipt.attempt_id);
        receipt.expected
    });
    let path = client.path.clone();
    drop(client);

    let _ = time::timeout(POLL_WAIT, async {
        loop {
            if poll(
                &path,
                &mut report,
                expected.as_ref(),
                prior,
                called_at_ms,
                &executable_sha256,
            )
            .await
            {
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;
    report.print(json)
}

async fn poll(
    path: &Path,
    report: &mut Report,
    expected: Option<&Build>,
    prior: Option<&str>,
    called_at_ms: u64,
    checksum: &str,
) -> bool {
    let Ok(client_info) = subc_transport::read_for_client(path) else {
        return false;
    };
    let Ok(mut client) = CkClient::connect(ResolvedConnection {
        path: path.to_owned(),
        info: client_info,
        others: vec![],
    })
    .await
    else {
        return false;
    };
    let Ok(described) = client
        .rpc_value(ClientControlRequest::ServerDescribe {})
        .await
    else {
        return false;
    };
    let counters = &described["counters"];
    let Some(identity) = read::<Identity>(&counters["daemon_identity"]) else {
        return false;
    };
    let Some(attempt) = read::<Attempt>(&counters["last_upgrade"]) else {
        return false;
    };
    if identity.pid != report.daemon_pid
        || !matches_attempt(&attempt, report.attempt_id.as_deref(), prior, called_at_ms)
    {
        return false;
    }

    let build_matches = match expected {
        Some(build) => identity.build == *build,
        // If the connection dropped before the daemon's `exec_started` reply
        // (which names the build it is starting), identify
        // the replacement by the hash sent with the request instead.
        None => identity.executable_sha256.as_deref() == Some(checksum),
    };
    report.record(identity, &attempt);
    if attempt.outcome == "failed" || !report.blocked.is_empty() {
        report.outcome = Outcome::Failed;
        report.reason = attempt
            .stage
            .or_else(|| (!report.blocked.is_empty()).then(|| "blocked".into()));
        return true;
    }
    if attempt.outcome != "upgraded" || !build_matches {
        return false;
    }
    let Some(modules) = attempt.modules else {
        return false;
    };
    let Ok(roster) = client
        .rpc_value(ClientControlRequest::SupervisorList {})
        .await
    else {
        return false;
    };
    if cohort_live(&modules, &roster["modules"]) {
        report.outcome = Outcome::Upgraded;
        return true;
    }
    false
}

fn matches_attempt(
    attempt: &Attempt,
    known: Option<&str>,
    prior: Option<&str>,
    called_at_ms: u64,
) -> bool {
    match known {
        Some(id) => attempt.attempt_id == id,
        None => Some(attempt.attempt_id.as_str()) != prior && attempt.started_at_ms >= called_at_ms,
    }
}

fn cohort_live(cohort: &[ModuleDisposition], roster: &Value) -> bool {
    let Some(roster) = roster.as_array() else {
        return false;
    };
    // supervisor.list's live field includes registration for protocol: subc,
    // but only process liveness for protocol: none.
    cohort
        .iter()
        .all(|module| match module.disposition.as_str() {
            "stopped" => true,
            "resumed" | "restarted" => roster.iter().any(|entry| {
                entry["module_id"] == module.id
                    && entry["state"] == "running"
                    && entry["live"] == true
            }),
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_upgrade_without_receipt_includes_the_call_millisecond_but_excludes_prior_id() {
        let mut attempt = Attempt {
            attempt_id: "on-the-clock-edge".into(),
            started_at_ms: 42,
            outcome: "upgraded".into(),
            stage: None,
            restarted: vec![],
            modules: Some(vec![]),
        };
        assert!(matches_attempt(&attempt, None, Some("prior"), 42));
        assert!(!matches_attempt(&attempt, None, Some("prior"), 43));
        attempt.attempt_id = "prior".into();
        assert!(!matches_attempt(&attempt, None, Some("prior"), 42));
        assert!(!matches_attempt(&attempt, Some("receipt"), None, 0));
    }

    #[test]
    fn live_upgrade_roster_requires_live_running_modules_but_not_stopped_ones() {
        let modules: Vec<ModuleDisposition> = serde_json::from_value(serde_json::json!([
            {"id":"a", "disposition":"resumed"},
            {"id":"b", "disposition":"restarted"},
            {"id":"c", "disposition":"stopped"},
        ]))
        .unwrap();
        let mut roster = serde_json::json!([
            {"module_id":"a","state":"running","live":true},
            {"module_id":"b","state":"running","live":false},
        ]);
        assert!(!cohort_live(&modules, &roster));
        roster[1]["live"] = true.into();
        assert!(cohort_live(&modules, &roster));
        roster[1]["state"] = "draining".into();
        assert!(!cohort_live(&modules, &roster));
    }
}
