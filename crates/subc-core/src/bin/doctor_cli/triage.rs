use std::{
    env,
    ffi::OsStr,
    fs,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process,
    time::UNIX_EPOCH,
};

use ck_ui::metrics::{format_age_seconds, format_bytes, scalar_text};
use cortexkit_log::segment_day;
use serde_json::{json, Value};
use subc_transport::{connection_file, CONNECTION_FILE_NAME};

use super::{format_age_from_epoch_ms_now, humanize_identifier, CkError, JsonOutput};

pub(super) const TRIAGE_LOG_MAX_BYTES: u64 = 64 * 1024;
const TRIAGE_LOG_TAIL_LINES: usize = 20;
const EXPECTED_DAEMON_BINARY: &str = "ck-subc";

pub(super) fn daemon_triage(
    override_path: Option<&Path>,
    json_output: JsonOutput,
) -> Result<(), CkError> {
    let env_named = env::var_os("SUBC_CONNECTION_FILE").filter(|value| !value.is_empty());
    let candidates = connection_file::discovery_candidates(override_path, env_named.as_deref());
    let run_dir = candidates
        .first()
        .and_then(|path| path.parent())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let report = collect_daemon_triage(&candidates, &run_dir);
    if json_output.enabled {
        json_output.print(&report.json)
    } else {
        print_daemon_triage(&report);
        Ok(())
    }?;
    Err(CkError::TriageExit {
        exit_code: report.exit_code,
    })
}

pub(super) struct TriageReport {
    pub(super) json: Value,
    pub(super) exit_code: i32,
    pub(super) text: String,
}

pub(super) fn collect_daemon_triage(candidates: &[PathBuf], run_dir: &Path) -> TriageReport {
    let mut start_locks = Vec::new();
    for candidate in candidates {
        let lock_path = triage_start_lock_path(candidate);
        start_locks.push(triage_file_fact(&lock_path));
    }

    let mut connection_candidates = Vec::new();
    let mut selected: Option<(PathBuf, Value, Option<u64>)> = None;
    for path in candidates {
        let mut fact = serde_json::Map::new();
        fact.insert("path".into(), json!(path.display().to_string()));
        let metadata = match fs::metadata(path) {
            Ok(metadata) => {
                fact.insert("status".into(), json!("present"));
                fact.insert("size_bytes".into(), json!(metadata.len()));
                fact.insert("mtime".into(), triage_mtime_fact(&metadata));
                Some(metadata)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fact.insert("status".into(), json!("absent"));
                fact.insert("finding".into(), json!("connection file absent"));
                None
            }
            Err(error) => {
                fact.insert("status".into(), json!("skipped"));
                fact.insert("skipped".into(), json!(format!("stat failed: {error}")));
                None
            }
        };
        if metadata.is_some() {
            match fs::read(path) {
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(value) => {
                        fact.insert("json".into(), json!("valid"));
                        let fields = triage_connection_fields(&value);
                        fact.insert("fields".into(), fields.clone());
                        let pid = value.get("pid").and_then(Value::as_u64);
                        if selected.is_none() {
                            selected = Some((path.clone(), value, pid));
                        }
                    }
                    Err(error) => {
                        fact.insert("json".into(), json!("invalid"));
                        fact.insert(
                            "finding".into(),
                            json!(format!("connection file JSON parse failure: {error}")),
                        );
                    }
                },
                Err(error) => {
                    fact.insert("status".into(), json!("skipped"));
                    fact.insert("skipped".into(), json!(format!("read failed: {error}")));
                }
            }
        }
        connection_candidates.push(Value::Object(fact));
    }

    let connection_fact = if let Some((path, value, pid)) = &selected {
        let fields = triage_connection_fields(value);
        let mut object = serde_json::Map::new();
        object.insert(
            "candidates".into(),
            Value::Array(connection_candidates.clone()),
        );
        object.insert("selected_path".into(), json!(path.display().to_string()));
        object.insert("fields".into(), fields);
        if let Some(port) = value
            .get("endpoints")
            .and_then(Value::as_array)
            .and_then(|endpoints| endpoints.first())
            .and_then(|endpoint| endpoint.get("port"))
        {
            object.insert("port".into(), port.clone());
        }
        if let Some(daemon_id) = value.get("daemon_id") {
            // The connection file stores daemon_id as a JSON byte array; render
            // it as hex so the identity is comparable against log lines and
            // other surfaces instead of appearing as a raw number list.
            let rendered = daemon_id
                .as_array()
                .map(|bytes| {
                    bytes
                        .iter()
                        .filter_map(Value::as_u64)
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                })
                .map(Value::from)
                .unwrap_or_else(|| daemon_id.clone());
            object.insert("daemon_id".into(), rendered);
        }
        if let Some(wire_version) = value.get("wire_version") {
            object.insert("wire_version".into(), wire_version.clone());
        }
        if let Some(pid) = pid {
            object.insert("pid".into(), json!(pid));
        }
        Value::Object(object)
    } else {
        json!({
            "candidates": connection_candidates.clone(),
            "status": "absent-or-unusable"
        })
    };

    let effective_run_dir = selected
        .as_ref()
        .and_then(|(path, _, _)| path.parent())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(run_dir);
    let pid = selected
        .as_ref()
        .and_then(|(_, _, pid)| *pid)
        .and_then(|pid| u32::try_from(pid).ok());
    let process_fact = triage_process_fact(pid);
    let log_path = triage_daemon_log_path(effective_run_dir);
    let log_fact = triage_log_fact(&log_path);
    let connection_present = selected.is_some()
        || connection_candidates
            .iter()
            .any(|candidate| candidate.get("status") == Some(&json!("present")));
    let parse_failure = connection_candidates
        .iter()
        .any(|candidate| candidate.get("json") == Some(&json!("invalid")));
    let fields_complete = selected.as_ref().is_some_and(|(_, value, _)| {
        triage_connection_fields(value)
            .get("port")
            .and_then(Value::as_object)
            .and_then(|value| value.get("present"))
            .and_then(Value::as_bool)
            == Some(true)
            && triage_connection_fields(value)
                .get("daemon_id")
                .and_then(Value::as_object)
                .and_then(|value| value.get("present"))
                .and_then(Value::as_bool)
                == Some(true)
            && triage_connection_fields(value)
                .get("wire_version")
                .and_then(Value::as_object)
                .and_then(|value| value.get("present"))
                .and_then(Value::as_bool)
                == Some(true)
    });
    let (verdict, reason, exit_code) = if !connection_present {
        ("daemon-appears-down", "no connection file", 2)
    } else if parse_failure && selected.is_none() {
        ("daemon-state-ambiguous", "connection file is malformed", 3)
    } else if !fields_complete {
        (
            "daemon-state-ambiguous",
            "connection file is missing required fields",
            3,
        )
    } else if process_fact.get("status") == Some(&json!("live")) {
        (
            "daemon-appears-live",
            "connection file is present and pid is alive",
            0,
        )
    } else if process_fact.get("status") == Some(&json!("dead")) {
        (
            "daemon-state-ambiguous",
            "fresh connection file conflicts with a dead pid",
            3,
        )
    } else {
        (
            "daemon-state-ambiguous",
            "pid liveness could not be established",
            3,
        )
    };
    let json_report = json!({
        "run_dir": effective_run_dir.display().to_string(),
        "start_lock": { "candidates": start_locks },
        "connection_file": connection_fact,
        "process_liveness": process_fact,
        "log_tail": log_fact,
        "verdict": { "status": verdict, "reason": reason }
    });
    let text = format!("run dir: {}\n", effective_run_dir.display());
    TriageReport {
        json: json_report,
        exit_code,
        text,
    }
}

fn triage_connection_fields(value: &Value) -> Value {
    let port = value
        .get("endpoints")
        .and_then(Value::as_array)
        .and_then(|endpoints| endpoints.first())
        .and_then(|endpoint| endpoint.get("port"));
    json!({
        "port": { "present": port.is_some(), "value": port.cloned().unwrap_or(Value::Bool(false)) },
        "daemon_id": { "present": value.get("daemon_id").is_some() },
        "wire_version": { "present": value.get("wire_version").is_some() }
    })
}

fn triage_file_fact(path: &Path) -> Value {
    match fs::metadata(path) {
        Ok(metadata) => json!({
            "path": path.display().to_string(),
            "status": "present",
            "size_bytes": metadata.len(),
            "mtime": triage_mtime_fact(&metadata)
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({
            "path": path.display().to_string(), "status": "absent", "finding": "start-lock absent"
        }),
        Err(error) => json!({
            "path": path.display().to_string(), "status": "skipped", "skipped": format!("stat failed: {error}")
        }),
    }
}

#[cfg(test)]
#[test]
fn daemon_triage_reports_a_held_live_start_lock_as_present() {
    let dir = cortexkit_test_support::ScratchDir::new("triage-held-start-lock");
    let connection = dir.join(CONNECTION_FILE_NAME);
    let lock = fs::File::create(triage_start_lock_path(&connection)).unwrap();
    lock.lock().unwrap();
    fs::write(&connection, serde_json::to_vec(&json!({
        "schema": 1, "wire_version": 1, "endpoints": [{"host": "127.0.0.1", "port": 8757}],
        "key": "fixture-key", "daemon_id": "daemon-test", "pid": process::id(),
    })).unwrap()).unwrap();
    let triage = collect_daemon_triage(&[connection], &dir);
    assert_eq!(triage.json["verdict"]["status"], "daemon-appears-live");
    let lock = &triage.json["start_lock"]["candidates"][0];
    assert_eq!(lock["status"], "present");
    assert!(lock.get("finding").is_none());
}

fn triage_mtime_fact(metadata: &fs::Metadata) -> Value {
    match metadata
        .modified()
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
    {
        Ok(age) => json!({ "status": "known", "age_seconds": age.as_secs() }),
        Err(error) => {
            json!({ "status": "skipped", "skipped": format!("mtime age unavailable: {error}") })
        }
    }
}

// bootstrap.rs:801-812 derives the advisory path as <connection-file>.start-lock
// in the connection file's parent; keep this disk-only copy aligned with that source.
fn triage_start_lock_path(connection_file_path: &Path) -> PathBuf {
    let file_name = connection_file_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| CONNECTION_FILE_NAME.into());
    let parent = connection_file_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent.join(format!("{file_name}.start-lock"))
}

fn triage_process_fact(pid: Option<u32>) -> Value {
    let Some(pid) = pid else {
        return json!({ "status": "skipped", "skipped": "no pid recovered from connection file or start-lock" });
    };
    // Platform split: `ps` is the honest probe on unix; Windows has no ps
    // executable (a PowerShell alias is not spawnable), so shelling it there
    // either fails to start or resolves to a Git-supplied ps.exe with
    // different flag semantics -- both misreport a live pid as dead/skipped.
    // tasklist ships with every Windows edition the fleet targets.
    #[cfg(not(windows))]
    let output = process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid=,comm="])
        .output();
    #[cfg(windows)]
    let output = process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    let Ok(output) = output else {
        return json!({ "pid": pid, "status": "skipped", "skipped": "process probe command could not be started" });
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // tasklist exits 0 with an INFO: prose line when the filter matches
    // nothing, so "no CSV row" is the not-running signal there; ps signals it
    // via exit status or empty output.
    #[cfg(windows)]
    let running = output.status.success() && stdout.starts_with('"');
    #[cfg(not(windows))]
    let running = output.status.success() && !stdout.is_empty();
    if !running {
        return json!({ "pid": pid, "status": "dead", "executable": { "status": "not-running" } });
    }
    #[cfg(windows)]
    let command = stdout
        .split('"')
        .nth(1)
        .unwrap_or(&stdout)
        .trim_end_matches(".exe")
        .to_string();
    #[cfg(not(windows))]
    let command = stdout;
    let executable = Path::new(command.split_whitespace().last().unwrap_or(&command))
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(&command);
    json!({
        "pid": pid,
        "status": "live",
        "executable": {
            "status": if executable == EXPECTED_DAEMON_BINARY { "match" } else { "mismatch" },
            "observed": executable,
            "expected": EXPECTED_DAEMON_BINARY
        }
    })
}

/// The daemon log a triage should tail: the newest dated segment
/// (`logs/subc.<YYYY-MM-DD>.log`, which current daemons write), else the single
/// undated `logs/subc.log` that older daemons wrote, else the older stderr
/// redirect beside the connection file.
///
/// An upgrade never deletes the undated file, it only stops writing to it, so
/// preferring it would show the tail from the day the daemon was upgraded as
/// though it were current. Segments are picked by the date in their name, the same rule
/// `ck module logs` uses; the fact reports the file's mtime so a stale choice is
/// visible rather than silent.
pub(super) fn triage_daemon_log_path(run_dir: &Path) -> PathBuf {
    let logs = run_dir.join("logs");
    let newest_segment = fs::read_dir(&logs).ok().and_then(|entries| {
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let day = segment_day("subc", name.to_str()?)?;
                Some((day, entry.path()))
            })
            .max_by_key(|(day, _)| *day)
            .map(|(_, path)| path)
    });
    if let Some(path) = newest_segment {
        return path;
    }
    let undated = logs.join("subc.log");
    if undated.exists() {
        return undated;
    }
    run_dir.join("subc.log")
}

pub(super) fn triage_log_fact(path: &Path) -> Value {
    let Ok(metadata) = fs::metadata(path) else {
        return if path.exists() {
            json!({ "path": path.display().to_string(), "status": "skipped", "skipped": "log metadata could not be read" })
        } else {
            json!({ "path": path.display().to_string(), "status": "absent", "finding": "log absent" })
        };
    };
    if metadata.len() == 0 {
        return json!({ "path": path.display().to_string(), "status": "empty", "finding": "log empty" });
    }
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "status": "skipped", "skipped": format!("log open failed: {error}") })
        }
    };
    let read_len = metadata.len().min(TRIAGE_LOG_MAX_BYTES);
    if file.seek(SeekFrom::End(-(read_len as i64))).is_err() {
        return json!({ "path": path.display().to_string(), "status": "skipped", "skipped": "log seek failed" });
    }
    let mut bytes = vec![0; read_len as usize];
    if let Err(error) = file.read_exact(&mut bytes) {
        return json!({ "path": path.display().to_string(), "status": "skipped", "skipped": format!("log read failed: {error}") });
    }
    let all = String::from_utf8_lossy(&bytes);
    let lines = all.lines().collect::<Vec<_>>();
    let tail = lines
        .iter()
        .rev()
        .take(TRIAGE_LOG_TAIL_LINES)
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    let truncated = metadata.len() > TRIAGE_LOG_MAX_BYTES;
    let mut fact = serde_json::Map::new();
    fact.insert("path".into(), json!(path.display().to_string()));
    fact.insert("status".into(), json!("present"));
    // A tail carries no age of its own: a file nothing has written for days
    // reads exactly like a current one unless the fact says when it was last
    // written.
    if let Some(modified_ms) = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as u64)
    {
        fact.insert("modified_at_ms".into(), json!(modified_ms));
    }
    fact.insert("lines".into(), json!(tail));
    fact.insert("tail_lines".into(), json!(tail.len()));
    if truncated {
        fact.insert(
            "summary".into(),
            json!(format!(
                "last {} lines (read capped at {} bytes)",
                TRIAGE_LOG_TAIL_LINES, TRIAGE_LOG_MAX_BYTES
            )),
        );
    } else {
        fact.insert(
            "summary".into(),
            json!(format!("showing {} of {} lines", tail.len(), lines.len())),
        );
    }
    Value::Object(fact)
}

fn print_daemon_triage(report: &TriageReport) {
    let verdict = &report.json["verdict"];
    let status = triage_string(verdict.get("status"));
    let reason = triage_string(verdict.get("reason"));
    let pid = report.json["process_liveness"]
        .get("pid")
        .and_then(Value::as_u64);
    let verdict_line = match (status, pid) {
        ("daemon-appears-live", Some(pid)) => {
            format!("daemon appears live: connection file present, pid {pid} alive")
        }
        ("daemon-appears-down", _) => format!("daemon appears down: {reason}"),
        ("daemon-state-ambiguous", _) => format!("daemon state ambiguous: {reason}"),
        (_, _) => format!("{}: {reason}", humanize_identifier(status)),
    };
    println!("{verdict_line}");
    println!("{}", report.text.trim_end());

    let connection = &report.json["connection_file"];
    println!("start-lock:");
    for fact in report.json["start_lock"]["candidates"]
        .as_array()
        .into_iter()
        .flatten()
    {
        print_triage_file_fact(fact, false);
    }
    println!("connection-file:");
    for fact in connection["candidates"].as_array().into_iter().flatten() {
        print_triage_file_fact(fact, true);
    }
    if let Some(path) = connection.get("selected_path").and_then(Value::as_str) {
        println!("  selected: {path}");
        for field in ["port", "daemon_id", "wire_version"] {
            if let Some(value) = connection.get(field) {
                println!("    {field}: {}", scalar_text(value));
            }
        }
    }

    println!("process-liveness:");
    println!(
        "  {}",
        triage_process_line(&report.json["process_liveness"])
    );
    println!("log-tail:");
    let log = &report.json["log_tail"];
    let written = log
        .get("modified_at_ms")
        .and_then(Value::as_u64)
        .map(|modified_ms| {
            format!(
                ", last written {}",
                format_age_from_epoch_ms_now(modified_ms)
            )
        })
        .unwrap_or_default();
    println!(
        "  {}: {}{written}",
        triage_string(log.get("path")),
        triage_string(log.get("status"))
    );
    if let Some(finding) = log.get("finding").and_then(Value::as_str) {
        println!("  finding: {finding}");
    }
    if let Some(skipped) = log.get("skipped").and_then(Value::as_str) {
        println!("  skipped: {skipped}");
    }
    if let Some(summary) = log.get("summary").and_then(Value::as_str) {
        println!("  {summary}");
    }
    if let Some(lines) = log.get("lines").and_then(Value::as_array) {
        for line in lines.iter().filter_map(Value::as_str) {
            println!("  {line}");
        }
    }
}

fn print_triage_file_fact(fact: &Value, include_connection_fields: bool) {
    println!(
        "  {}: {}",
        triage_string(fact.get("path")),
        triage_string(fact.get("status"))
    );
    if let Some(size) = fact.get("size_bytes").and_then(Value::as_u64) {
        println!("    size: {}", format_bytes(size));
    }
    if let Some(mtime) = fact.get("mtime") {
        let rendered = mtime
            .get("age_seconds")
            .and_then(Value::as_u64)
            .map(format_age_seconds)
            .or_else(|| {
                mtime
                    .get("skipped")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());
        println!("    modified: {rendered}");
    }
    if let Some(json_status) = fact.get("json").and_then(Value::as_str) {
        println!("    parses as JSON: {json_status}");
    }
    if include_connection_fields {
        if let Some(fields) = fact.get("fields").and_then(Value::as_object) {
            let rendered = ["daemon_id", "port", "wire_version"]
                .iter()
                .map(|field| {
                    let field_value = fields.get(*field);
                    let present = field_value
                        .and_then(|value| value.get("present"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let status = if present { "present" } else { "absent" };
                    let value = field_value
                        .and_then(|value| value.get("value"))
                        .filter(|value| !value.is_boolean())
                        .map(|value| format!(" ({})", scalar_text(value)))
                        .unwrap_or_default();
                    format!("{field} {status}{value}")
                })
                .collect::<Vec<_>>()
                .join(" · ");
            println!("    fields: {rendered}");
        }
    }
    if let Some(finding) = fact.get("finding").and_then(Value::as_str) {
        println!("    finding: {finding}");
    }
    if let Some(skipped) = fact.get("skipped").and_then(Value::as_str) {
        println!("    skipped: {skipped}");
    }
}

fn triage_process_line(process: &Value) -> String {
    let status = process
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let pid = process
        .get("pid")
        .and_then(Value::as_u64)
        .map(|pid| format!("pid {pid} "))
        .unwrap_or_default();
    let mut line = match status {
        "live" => format!("{pid}alive"),
        "dead" => format!("{pid}stopped"),
        other => format!("{pid}{}", humanize_identifier(other)),
    };
    if let Some(executable) = process.get("executable").and_then(Value::as_object) {
        let observed = executable
            .get("observed")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let expected = executable
            .get("expected")
            .and_then(Value::as_str)
            .unwrap_or(EXPECTED_DAEMON_BINARY);
        let agreement = match executable.get("status").and_then(Value::as_str) {
            Some("match") => "matches",
            Some("mismatch") => "differs from",
            Some("not-running") => "was expected to be",
            _ => "could not be compared with",
        };
        line.push_str(&format!("; executable {observed} {agreement} {expected}"));
    }
    if let Some(skipped) = process.get("skipped").and_then(Value::as_str) {
        line.push_str(&format!("; {skipped}"));
    }
    line
}

fn triage_string(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("-")
}
