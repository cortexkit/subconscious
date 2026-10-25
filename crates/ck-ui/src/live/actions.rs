//! Lifecycle following shared by the full-screen app and command-line actions.

use std::time::Duration;

use futures_util::future::BoxFuture;
use subc_control::{CatalogEntry, SupervisorHealthStatus};
use subc_protocol::manifest::{SelfSignalKind, SignalAnchor};

use super::{action_progress, LiveProcess, State};
use crate::model::{ActionKind, ActionPhase};
use crate::time::Timestamp;

pub const ACTION_WATCH: Duration = Duration::from_secs(120);

pub struct Reading {
    pub state: State,
    /// A sum only when every declared busy gauge was reported by this process.
    pub busy: Option<u64>,
    pub busy_declared: bool,
    pub budget_ms: u64,
}

pub trait ActionSource: Sync {
    type Error;
    fn read(&self) -> BoxFuture<'_, Result<Reading, Self::Error>>;
    fn send(
        &self,
        kind: ActionKind,
        module: &str,
        drain_timeout_ms: Option<u64>,
    ) -> BoxFuture<'_, Result<bool, Self::Error>>;
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub ok: bool,
    pub state: String,
    pub plain: String,
    pub inline: String,
    pub detail: String,
    /// Whether a new process was observed spawning after the action began.
    pub new_process: bool,
}

/// The gauges are named by the module, not inferred from familiar metric names.
pub fn busy_gauges(catalog: &[CatalogEntry], module: &str) -> Vec<String> {
    let mut gauges = Vec::new();
    for signal in catalog
        .iter()
        .filter(|e| e.module_id == module)
        .flat_map(|e| e.self_signals.iter().flatten())
    {
        if signal.kind == SelfSignalKind::Busy {
            if let SignalAnchor::HealthGauges { gauges: names } = &signal.anchored_to {
                gauges.extend(names.iter().cloned());
            }
        }
    }
    gauges.sort();
    gauges.dedup();
    gauges
}

pub fn busy_count(gauges: &[String], metrics: Option<&serde_json::Value>) -> Option<u64> {
    if gauges.is_empty() {
        return None;
    }
    gauges.iter().try_fold(0u64, |sum, name| {
        let value = metrics?.get(name)?.as_u64()?;
        sum.checked_add(value)
    })
}

pub struct Watch {
    kind: ActionKind,
    module: String,
    before: Option<LiveProcess>,
    started: Timestamp,
    initial_busy: Option<u64>,
    busy_declared: bool,
    last_busy: Option<u64>,
    budget_ms: u64,
    last_state: String,
    pid: Option<u32>,
    new_process: bool,
}

impl Watch {
    pub fn new(
        kind: ActionKind,
        module: &str,
        reading: &Reading,
        started: Timestamp,
        drain: Option<u64>,
    ) -> Self {
        Self {
            kind,
            module: module.into(),
            before: reading.state.processes.get(module).copied(),
            started,
            initial_busy: reading.busy,
            busy_declared: reading.busy_declared,
            last_busy: reading.busy,
            budget_ms: drain.unwrap_or(reading.budget_ms),
            last_state: reading
                .state
                .roster
                .value
                .iter()
                .flatten()
                .find(|e| e.module_id == module)
                .map(|e| e.state.clone())
                .unwrap_or_else(|| "unknown".into()),
            pid: reading.state.processes.get(module).map(|p| p.pid),
            new_process: false,
        }
    }

    pub fn initial_line(&self) -> String {
        if self.kind == ActionKind::Start {
            format!("{} starting", self.module)
        } else {
            self.drain_line()
        }
    }

    fn drain_line(&self) -> String {
        let count = if self.busy_declared {
            self.last_busy
                .map(|n| format!(" · {n} in flight"))
                .unwrap_or_else(|| " · unknown in flight".into())
        } else {
            String::new()
        };
        format!(
            "{} draining{count} · budget {} s",
            self.module,
            crate::time::to_fixed(self.budget_ms as f64 / 1000.0, 1)
        )
    }

    pub fn progress(
        &mut self,
        reading: &Reading,
        now: Timestamp,
        strict: bool,
    ) -> (ActionPhase, String, Option<Outcome>) {
        let current = reading.state.processes.get(&self.module).copied();
        // A zero from a replacement cannot prove that the old process drained.
        if self.before.is_some() && current == self.before {
            self.last_busy = reading.busy;
        }
        self.pid = current.map(|p| p.pid);
        self.new_process =
            super::fresh_action_process(current, self.before, self.started).is_some();
        self.last_state = reading
            .state
            .roster
            .value
            .iter()
            .flatten()
            .find(|e| e.module_id == self.module)
            .map(|e| e.state.clone())
            .unwrap_or_else(|| "unknown".into());
        if current.is_some() && matches!(self.last_state.as_str(), "stopped" | "disabled") {
            self.last_state = "still running".into();
        }
        let (mut phase, mut done) = action_progress(
            &reading.state,
            self.kind,
            &self.module,
            self.before,
            self.started,
            now,
        );
        // An unchanged, already-running process can satisfy start, but not restart.
        if let Some(current) = current.filter(|_| self.kind == ActionKind::Start && done.is_none())
        {
            let (existing_phase, existing) = action_progress(
                &reading.state,
                self.kind,
                &self.module,
                None,
                current.spawned_at,
                now,
            );
            phase = existing_phase;
            done = existing;
        }
        let line = match phase {
            ActionPhase::Asking => format!("{} starting", self.module),
            ActionPhase::Draining => self.drain_line(),
            ActionPhase::Stopped => format!("{} stopping", self.module),
            ActionPhase::Starting { pid } => format!("{} starting · pid {pid}", self.module),
        };
        let outcome = done.and_then(|(ok, text)| {
            if ok && strict && text.ends_with("health unknown") {
                self.last_state = "running, health unknown".into();
                return None;
            }
            let state = if ok {
                if self.kind == ActionKind::Stop {
                    "stopped"
                } else if text.ends_with("health unknown") {
                    "health unknown"
                } else {
                    "healthy"
                }
            } else {
                match reading.state.health_of(&self.module).map(|h| h.status) {
                    Some(SupervisorHealthStatus::Degraded) => "degraded",
                    Some(SupervisorHealthStatus::Failing) => "failing",
                    Some(SupervisorHealthStatus::Unresponsive) => "unresponsive",
                    _ => "health unknown",
                }
            };
            self.last_state = state.into();
            Some(self.outcome(ok, state, now, &text))
        });
        (phase, line, outcome)
    }

    fn outcome(&self, ok: bool, state: &str, now: Timestamp, detail: &str) -> Outcome {
        let already_running =
            self.kind == ActionKind::Start && self.pid.is_some() && !self.new_process;
        let detail = if already_running {
            detail.replacen(
                &format!("{} started", self.module),
                &format!("{} already running", self.module),
                1,
            )
        } else {
            detail.to_string()
        };
        let past = match self.kind {
            ActionKind::Restart => "restarted",
            ActionKind::Start => "started",
            ActionKind::Stop => "stopped",
        };
        let verb = if already_running {
            "already running"
        } else if !ok && (self.pid.is_none() || self.pid == self.before.map(|p| p.pid)) {
            self.kind.verb()
        } else {
            past
        };
        let pid = self.pid.map(|p| format!(" · pid {p}")).unwrap_or_default();
        let plain = if self.kind == ActionKind::Stop && ok {
            format!("{} stopped", self.module)
        } else {
            format!("{} {verb} · {state}{pid}", self.module)
        };
        let elapsed = crate::time::to_fixed(now.secs_since(self.started).max(0.0), 1);
        let finished = if self.kind != ActionKind::Start && self.last_busy == Some(0) {
            self.initial_busy
                .filter(|n| *n > 0)
                .map(|n| format!(" · {n} runs finished first"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let inline = if ok && already_running {
            format!("✓ {plain}")
        } else if ok && self.kind == ActionKind::Stop {
            format!("✓ {} stopped in {elapsed} s{finished}", self.module)
        } else if ok {
            format!(
                "✓ {} {verb} in {elapsed} s{finished} · {}",
                self.module,
                detail
                    .split_once(" · ")
                    .map_or(detail.as_str(), |(_, rest)| rest)
            )
        } else if detail.starts_with(&self.module) {
            format!("✗ {detail}")
        } else {
            format!("✗ {plain} · {detail}")
        };
        Outcome {
            ok,
            state: state.into(),
            plain,
            inline,
            detail,
            new_process: self.new_process,
        }
    }

    pub fn deadline(&self, now: Timestamp) -> Outcome {
        let mut outcome = self.outcome(false, &self.last_state, now, "watch deadline passed");
        outcome.detail = format!("{} · watch deadline passed", outcome.plain);
        outcome
    }
}

/// Sends exactly one action. Dropping the future stops observation, not the daemon's work.
pub async fn run_action<S: ActionSource>(
    source: &S,
    kind: ActionKind,
    module: &str,
    drain_timeout_ms: Option<u64>,
    strict: bool,
    mut progress: impl FnMut(ActionPhase, String),
) -> Result<Outcome, S::Error> {
    let initial = source.read().await?;
    let started = Timestamp::system_now();
    let mut watch = Watch::new(kind, module, &initial, started, drain_timeout_ms);
    progress(
        if kind == ActionKind::Start {
            ActionPhase::Asking
        } else {
            ActionPhase::Draining
        },
        watch.initial_line(),
    );
    let deadline = tokio::time::Instant::now() + ACTION_WATCH;
    let mut stopped = false;
    let mut starting_pid = None;
    let mut last_drain = watch.initial_line();
    let following = async {
        let sending = source.send(kind, module, drain_timeout_ms);
        tokio::pin!(sending);
        let mut acknowledged = false;
        loop {
            // Stop can acknowledge only after the drain. Its separate control
            // connection must not prevent readings during that wait.
            tokio::select! {
                biased;
                reply = &mut sending, if !acknowledged => {
                    let _applied = reply?;
                    acknowledged = true;
                }
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
            let reading = source.read().await?;
            let (phase, line, done) = watch.progress(&reading, Timestamp::system_now(), strict);
            match phase {
                ActionPhase::Stopped if !stopped => {
                    stopped = true;
                    progress(ActionPhase::Stopped, line);
                }
                ActionPhase::Starting { pid } if starting_pid != Some(pid) => {
                    if kind == ActionKind::Restart && !stopped {
                        stopped = true;
                        progress(ActionPhase::Stopped, format!("{module} stopping"));
                    }
                    starting_pid = Some(pid);
                    progress(ActionPhase::Starting { pid }, line);
                    progress(
                        ActionPhase::Starting { pid },
                        format!("{module} waiting for health"),
                    );
                }
                ActionPhase::Draining
                    if !stopped && starting_pid.is_none() && line != last_drain =>
                {
                    last_drain = line.clone();
                    progress(ActionPhase::Draining, line);
                }
                _ => {}
            }
            if let Some(done) = done {
                if acknowledged {
                    return Ok(done);
                }
            }
        }
    };
    match tokio::time::timeout_at(deadline, following).await {
        Ok(result) => result,
        Err(_) => Ok(watch.deadline(Timestamp::system_now())),
    }
}
