use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use subc_control::{
    StderrCaptureState, StderrTail, StderrTailEntry, SupervisorEntry, SupervisorHealthEntry,
    SupervisorHealthStatus, TerminalDisposition, TerminalEntry, TerminalExitKind, TerminalHistory,
};

use super::{
    host::{DoctorDaemon, RpcError, RpcResult},
    report::{
        one_line, Finding, Layer, LogTailRecord, Severity, TailEntry, TailSource, TailStatus,
    },
    DoctorHost, LogFallback, LogFallbackRead,
};

pub(super) const LIST: &str = "supervisor.list";
pub(super) const HEALTH: &str = "supervisor.health";
const TERMINALS: &str = "supervisor.terminals";
const TAIL: &str = "supervisor.stderr_tail";
const LOG_FALLBACK: &str = "log fallback";

pub(super) fn collect_modules(
    daemon: Arc<dyn DoctorDaemon>,
    modules: &[SupervisorEntry],
    cutoff: Instant,
    budget: Duration,
) -> ModuleReads {
    let mut reads = ModuleReads(modules.iter().map(|_| ModuleRead::default()).collect());
    if modules.is_empty() {
        return reads;
    }

    let (sender, receiver) = mpsc::channel();
    let mut jobs = Vec::new();
    for (index, module) in modules.iter().enumerate() {
        if module.enabled {
            jobs.push((index, module.module_id.clone(), JobKind::Terminals));
        }
    }
    let max_bytes = (64 * 1024 / modules.len()) as u32;
    for (index, module) in modules.iter().enumerate() {
        jobs.push((index, module.module_id.clone(), JobKind::Tail));
    }
    let (jobs, next) = (Arc::new(jobs), Arc::new(AtomicUsize::new(0)));
    for _ in 0..8.min(jobs.len()) {
        let (sender, jobs, next, daemon) =
            (sender.clone(), jobs.clone(), next.clone(), daemon.clone());
        thread::spawn(move || {
            while Instant::now() < cutoff {
                let Some((index, module, kind)) = jobs.get(next.fetch_add(1, Ordering::Relaxed))
                else {
                    break;
                };
                let start = Instant::now();
                if start >= cutoff {
                    break;
                }
                let deadline = cutoff.min(start + budget);
                let result = match kind {
                    JobKind::Terminals => JobResult::Terminals(daemon.terminals(module, deadline)),
                    JobKind::Tail => {
                        JobResult::Tail(daemon.stderr_tail(module, 20, max_bytes, deadline))
                    }
                };
                if Instant::now() <= deadline && sender.send((*index, result)).is_err() {
                    break;
                }
            }
        });
    }
    drop(sender);
    while let Ok((index, result)) =
        receiver.recv_timeout(cutoff.saturating_duration_since(Instant::now()))
    {
        match result {
            JobResult::Terminals(result) => reads.0[index].terminals = Some(result),
            JobResult::Tail(result) => reads.0[index].tail = Some(result),
        }
    }
    reads
}

enum JobKind {
    Terminals,
    Tail,
}
enum JobResult {
    Terminals(RpcResult<TerminalHistory>),
    Tail(RpcResult<StderrTail>),
}

#[derive(Default)]
pub(super) struct ModuleReads(Vec<ModuleRead>);

#[derive(Default)]
struct ModuleRead {
    terminals: Option<RpcResult<TerminalHistory>>,
    tail: Option<RpcResult<StderrTail>>,
}

pub(super) struct ModuleFindings {
    pub(super) findings: Vec<Finding>,
    pub(super) tails: Vec<LogTailRecord>,
    pub(super) lost_at: Option<&'static str>,
}

impl ModuleReads {
    pub(super) fn findings(
        self,
        modules: &[SupervisorEntry],
        health: &[SupervisorHealthEntry],
        now_ms: u64,
    ) -> ModuleFindings {
        let mut result = ModuleFindings {
            findings: Vec::new(),
            tails: Vec::new(),
            lost_at: None,
        };
        let health = health
            .iter()
            .map(|health| (health.module_id.as_str(), health))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (module, reads) in modules.iter().zip(self.0) {
            let id = module.module_id.as_str();
            let tail = match reads.tail.unwrap_or(Err(RpcError::TimedOut)) {
                Ok(tail) => stderr_tail(id, tail),
                Err(error) => {
                    let (status, reason) = match error {
                        RpcError::TimedOut => (TailStatus::RequestTimedOut, None),
                        RpcError::Failed(message) => (TailStatus::RequestFailed, Some(message)),
                        RpcError::ConnectionLost => {
                            result.lost_at.get_or_insert(TAIL);
                            (
                                TailStatus::RequestFailed,
                                Some("The pass's daemon connection was lost".into()),
                            )
                        }
                    };
                    LogTailRecord {
                        reason,
                        ..LogTailRecord::empty(id, TailSource::StderrTail, status)
                    }
                }
            };
            let history = if module.enabled {
                match reads.terminals.unwrap_or(Err(RpcError::TimedOut)) {
                    Ok(history) => Some(history),
                    Err(error) => {
                        if matches!(error, RpcError::ConnectionLost) {
                            result.lost_at = Some(TERMINALS);
                        }
                        result
                            .findings
                            .push(rpc_finding(TERMINALS, Some(id), error));
                        None
                    }
                }
            } else {
                None
            };
            result.findings.extend(classify(
                module,
                health.get(id).copied(),
                history.as_ref(),
                &tail,
                now_ms,
            ));
            result.tails.push(tail);
        }
        result
    }
}

pub(super) fn rpc_finding(check: &'static str, module: Option<&str>, error: RpcError) -> Finding {
    let mut severity = if check == LIST {
        Severity::Error
    } else {
        Severity::Warning
    };
    let (timed_out, message) = match error {
        RpcError::TimedOut => {
            if check != HEALTH {
                severity = Severity::Error;
            }
            (
                check != HEALTH,
                "The check did not answer before its deadline".into(),
            )
        }
        RpcError::Failed(error) => (false, error),
        RpcError::ConnectionLost => (false, "The pass's daemon connection was lost".into()),
    };
    Finding::check(Layer::Module, severity, timed_out, check, module, message)
}

pub(super) fn classify(
    module: &SupervisorEntry,
    health: Option<&SupervisorHealthEntry>,
    history: Option<&TerminalHistory>,
    tail: &LogTailRecord,
    now_ms: u64,
) -> Vec<Finding> {
    let id = module.module_id.as_str();
    let finding = |key, severity, summary: String, mut evidence: Value| {
        evidence["tail"] = json!(tail.preview());
        Finding::new(key, Layer::Module, severity, one_line(&summary), evidence).for_module(id)
    };

    if !module.enabled {
        return if module.configured_enabled == Some(true) {
            vec![finding(
                "module.stopped_by_operator",
                Severity::Info,
                format!("Module {id} was stopped by an operator; a daemon restart undoes the stop"),
                json!({"state": module.state, "enabled": false, "configured_enabled": true}),
            )]
        } else {
            Vec::new()
        };
    }

    let recent = history
        .into_iter()
        .flat_map(|history| &history.entries)
        .filter(|entry| (now_ms.saturating_sub(10 * 60 * 1000)..=now_ms).contains(&entry.at_ms))
        .collect::<Vec<_>>();
    if let (Some(count @ 3..), Some(window)) = (module.restart_count, module.restart_window_secs) {
        if recent
            .iter()
            .filter(|entry| is_crash(entry))
            .take(3)
            .count()
            == 3
        {
            let exits: Vec<_> = recent.iter().map(|entry| {
                json!({"at_ms": entry.at_ms, "code": entry.exit_code, "signal": entry.exit_signal, "kind": entry.exit_kind})
            }).collect();
            return vec![finding(
                "module.crash_loop",
                Severity::Error,
                format!("Module {id} crashed at least three times in ten minutes"),
                json!({
                    "restart_count": count, "max_restarts": module.max_restarts, "restart_window_secs": window,
                    "exits": exits, "lifetime_restarts": module.lifetime_restarts,
                }),
            )];
        }
    }

    let mut findings = Vec::new();
    if let Some(reason) = &module.admission_refused {
        let explanation = if reason == "registration_channel_unsupported" {
            " (the module lacks its support declaration)"
        } else {
            ""
        };
        findings.push(finding(
            "module.admission_refused",
            Severity::Error,
            format!("Daemon refused to admit module {id}: {reason}{explanation}"),
            json!({"reason": reason}),
        ));
    }

    let state = module.state.as_str();
    let state_finding = match state {
        "running" | "stopped" | "disabled" => None,
        "failed" if module.admission_refused.is_some() => None,
        "failed" => Some(("module.failed", Severity::Error)),
        "unresponsive" => Some(("module.unresponsive", Severity::Error)),
        "starting" => Some(("module.starting", Severity::Info)),
        "restarting" => Some(("module.restarting", Severity::Info)),
        "draining" => Some(("module.draining", Severity::Info)),
        _ => Some(("module.unknown_state", Severity::Warning)),
    };
    if let Some((code, severity)) = state_finding {
        let summary = if code == "module.unknown_state" {
            format!("Module {id} reported an unknown state {state:?}")
        } else {
            format!("Module {id} is {state}")
        };
        findings.push(finding(code, severity, summary, json!({"state": state})));
    }

    if state == "running" && module.last_probe_ms.is_some() {
        let verdict = match module.health {
            SupervisorHealthStatus::Failing | SupervisorHealthStatus::Unresponsive => {
                Some(("module.unhealthy", Severity::Error, "unhealthy"))
            }
            SupervisorHealthStatus::Degraded => {
                Some(("module.degraded", Severity::Warning, "degraded"))
            }
            _ => None,
        };
        if let Some((code, severity, description)) = verdict {
            findings.push(finding(code, severity, format!("Module {id} is {description}"), json!({
                "state": state, "health": module.health, "last_probe_ms": module.last_probe_ms,
                "detail": health.and_then(|entry| entry.detail.as_ref()),
                "consecutive_failures": health.map(|entry| entry.consecutive_failures),
                "last_action": health.and_then(|entry| entry.last_action.as_ref()).map(snake_case_value),
            })));
        }
    }

    if !module.live && state != "starting" && findings.is_empty() {
        let last_exit = module.last_exit_ms.map(|at_ms| {
            json!({
                "at_ms": at_ms, "code": module.last_exit_code, "signal": module.last_exit_signal,
                "kind": module.last_exit_kind,
            })
        });
        findings.push(finding(
            "module.not_serving",
            Severity::Warning,
            format!("Module {id} is not serving"),
            json!({"state": state, "last_exit": last_exit}),
        ));
    }
    findings
}

fn snake_case_value(value: impl serde::Serialize) -> Value {
    match json!(value) {
        Value::String(text) => Value::String(text.replace('-', "_")),
        other => other,
    }
}

fn is_crash(entry: &TerminalEntry) -> bool {
    if matches!(
        entry.disposition,
        TerminalDisposition::Stopped
            | TerminalDisposition::Disabled
            | TerminalDisposition::DaemonShutdown
    ) {
        return false;
    }
    match entry.exit_kind.as_ref() {
        Some(TerminalExitKind::Crash) => true,
        None => matches!(
            entry.disposition,
            TerminalDisposition::Restarting | TerminalDisposition::Failed
        ),
        _ => false,
    }
}

pub(super) fn stderr_tail(module: &str, tail: StderrTail) -> LogTailRecord {
    let (status, reason) = match tail.capture {
        StderrCaptureState::Captured => (TailStatus::Captured, None),
        StderrCaptureState::Incomplete { reason } => (TailStatus::Incomplete, Some(reason)),
        StderrCaptureState::NotCaptured { reason } => (TailStatus::NotCaptured, Some(reason)),
        StderrCaptureState::Unknown { tag, .. } => (TailStatus::Unknown, Some(tag)),
    };
    let entries = tail
        .entries
        .into_iter()
        .map(|entry| match entry {
            StderrTailEntry::Line {
                text,
                at_ms,
                truncated,
            } => TailEntry::Line {
                text,
                at_ms,
                truncated,
            },
            StderrTailEntry::ProcessStart => TailEntry::ProcessStart,
            StderrTailEntry::Unknown { .. } => TailEntry::Unknown,
        })
        .collect();
    LogTailRecord {
        reason,
        dropped_lines: Some(tail.dropped_lines),
        entries,
        ..LogTailRecord::empty(module, TailSource::StderrTail, status)
    }
}

pub(super) struct OfflineLogs {
    receiver: mpsc::Receiver<(Instant, LogEvent)>,
    deadline: Instant,
}

enum LogEvent {
    Modules(Vec<String>),
    Tail(LogTailRecord),
    Failed(String),
    Done,
}

impl OfflineLogs {
    pub(super) fn start(
        host: Arc<dyn DoctorHost>,
        logs: Arc<dyn LogFallback>,
        budget: Duration,
    ) -> Self {
        let deadline = Instant::now() + budget;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let send = |event| sender.send((Instant::now(), event)).is_ok();
            let mut modules = match host.configured_modules() {
                Ok(modules) => modules,
                Err(error) => {
                    send(LogEvent::Failed(error));
                    return;
                }
            };
            modules.sort_unstable();
            modules.dedup();
            if !send(LogEvent::Modules(modules.clone())) {
                return;
            }
            for module in modules {
                if Instant::now() >= deadline {
                    return;
                }
                if !send(LogEvent::Tail(log_record(&module, logs.tail(&module)))) {
                    return;
                }
            }
            send(LogEvent::Done);
        });
        Self { receiver, deadline }
    }

    fn collect(self) -> (Vec<LogTailRecord>, Option<Finding>) {
        let mut records = std::collections::BTreeMap::new();
        let mut completed = false;
        while let Ok((finished, event)) = self
            .receiver
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
        {
            if finished > self.deadline {
                break;
            }
            match event {
                LogEvent::Modules(modules) => {
                    for module in modules {
                        let record =
                            LogTailRecord::empty(&module, TailSource::LogFile, TailStatus::NotRead);
                        records.insert(module, record);
                    }
                }
                LogEvent::Tail(record) => {
                    records.insert(record.module.clone(), record);
                }
                LogEvent::Failed(error) => {
                    return (
                        Vec::new(),
                        Some(Finding::check(
                            Layer::Module,
                            Severity::Warning,
                            false,
                            LOG_FALLBACK,
                            None,
                            error,
                        )),
                    )
                }
                LogEvent::Done => {
                    completed = true;
                    break;
                }
            }
        }
        let finding = (!completed).then(|| {
            let timed_out = Instant::now() >= self.deadline || !records.is_empty();
            Finding::check(
                Layer::Module,
                if timed_out {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                timed_out,
                LOG_FALLBACK,
                None,
                if timed_out {
                    "The log fallback did not finish before its deadline"
                } else {
                    "The log fallback worker did not finish"
                },
            )
        });
        (records.into_values().collect(), finding)
    }
}

pub(super) fn offline(logs: Option<OfflineLogs>) -> ModuleFindings {
    let (tails, failure) = logs.map_or_else(|| (Vec::new(), None), OfflineLogs::collect);
    let previews = tails.iter().map(LogTailRecord::preview).collect::<Vec<_>>();
    let mut findings = vec![Finding::new(
        "module.daemon_unavailable",
        Layer::Module,
        Severity::Info,
        "The daemon did not answer; module status is unavailable",
        json!({"tails": previews}),
    )];
    findings.extend(failure);
    ModuleFindings {
        findings,
        tails,
        lost_at: None,
    }
}

fn log_record(module: &str, read: LogFallbackRead) -> LogTailRecord {
    let mut record = LogTailRecord::empty(module, TailSource::LogFile, TailStatus::Absent);
    record.reason = read.run_folder_warning;
    match read.tail {
        Ok(Some(tail)) => {
            record.status = TailStatus::Read;
            record.truncated = tail.truncated;
            record.entries = tail
                .entries
                .into_iter()
                .map(|text| TailEntry::Line {
                    text,
                    at_ms: None,
                    truncated: false,
                })
                .collect();
        }
        Ok(None) => (),
        Err(error) => {
            record.status = TailStatus::Unreadable;
            record.reason = Some(match record.reason {
                Some(warning) => format!("{error}; {warning}"),
                None => error,
            });
        }
    }
    record
}
