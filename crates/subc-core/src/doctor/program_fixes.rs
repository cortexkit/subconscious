//! Consented fixes and restoration of any supervisor stop the run owns.

use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// The caller's terminal dialogue. An unreadable answer counts as no.
pub trait Prompter {
    fn interactive(&self) -> bool;
    fn confirm(&self, module: &str, finding: &cortexkit_doctor::Finding) -> bool;
}

/// Runtime inputs for a program-enabled doctor run.
pub struct Programs<'a> {
    pub prompter: &'a dyn Prompter,
    pub interrupt: &'a Interrupt,
    pub operator_budget: Duration,
}

/// Protection shared with the caller's signal listener. It installs no handlers.
#[derive(Default)]
pub struct Interrupt(Mutex<InterruptState>);

#[derive(Default)]
struct InterruptState {
    signal: Option<i32>,
    setup_failed: bool,
    next_guard: u64,
    guards: BTreeMap<u64, Wait>,
}

#[derive(Clone, Copy)]
struct Wait {
    operation: &'static str,
    deadline: Instant,
}

/// What the signal listener can tell the person after latching a signal.
#[derive(Debug, Clone, Copy)]
pub struct WaitStatus {
    pub operation: &'static str,
    pub remaining: Duration,
}

impl Interrupt {
    /// Latch the first signal during protected work. `None` tells the caller
    /// to exit immediately; a latched signal is never replaced or cleared.
    pub fn on_signal(&self, signal: i32) -> Option<WaitStatus> {
        let mut state = self.0.lock().unwrap();
        let active = state
            .guards
            .values()
            .max_by_key(|wait| wait.deadline)
            .copied();
        if state.signal.is_none() {
            state.signal = active.map(|_| signal);
        }
        state.signal.map(|_| match active {
            Some(wait) => WaitStatus {
                operation: wait.operation,
                remaining: wait.deadline.saturating_duration_since(Instant::now()),
            },
            _ => WaitStatus {
                operation: "the report",
                remaining: Duration::ZERO,
            },
        })
    }

    pub fn signal(&self) -> Option<i32> {
        self.0.lock().unwrap().signal
    }

    /// Record a registration failure without disabling check protection.
    pub fn signal_setup_failed(&self) {
        self.0.lock().unwrap().setup_failed = true;
    }

    /// Protect a new operation, unless a signal has already been latched.
    pub fn protect(&self, operation: &'static str, deadline: Instant) -> Option<Guard<'_>> {
        let mut state = self.0.lock().unwrap();
        if state.signal.is_some() {
            return None;
        }
        let id = state.next_guard;
        state.next_guard += 1;
        state.guards.insert(
            id,
            Wait {
                operation,
                deadline,
            },
        );
        Some(Guard {
            interrupt: self,
            id,
        })
    }

    #[cfg(unix)]
    fn setup_failed(&self) -> bool {
        self.0.lock().unwrap().setup_failed
    }
}

/// Protects a single running operation and releases it when the operation ends.
pub struct Guard<'a> {
    interrupt: &'a Interrupt,
    id: u64,
}

impl Guard<'_> {
    #[cfg(unix)]
    fn update(&self, operation: &'static str, deadline: Instant) {
        let mut state = self.interrupt.0.lock().unwrap();
        state.guards.insert(
            self.id,
            Wait {
                operation,
                deadline,
            },
        );
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.interrupt.0.lock().unwrap().guards.remove(&self.id);
    }
}

#[cfg(unix)]
pub(super) use unix::*;

#[cfg(unix)]
mod unix {
    use super::{Guard, Programs};
    use crate::doctor::{
        host::{DoctorDaemon, RpcError, SetEnabledError},
        program_checks::{diagnostic, CheckedProgram, ProgramConfig},
        run::{check_rpc, launch},
        DoctorHost, Finding, ModuleFix, ModuleFixDecision, ModuleFixOutcome, ModuleFixResult,
        ModuleFixSkip, ModuleRestart, Severity,
    };
    use cortexkit_doctor::{runner, Crash, FixClass, FixResult, RunError};
    use rustix::process::Signal;
    use serde_json::json;
    use std::{
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };
    use subc_control::SupervisorEntry;

    const POLL_LIMIT: Duration = Duration::from_secs(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(100);
    const RECONNECT_LIMIT: Duration = Duration::from_secs(10);
    pub(in crate::doctor) const DRAIN_RESPONSE_MARGIN: Duration = Duration::from_secs(2);

    pub(in crate::doctor) fn offered(
        checked: &[CheckedProgram],
    ) -> Vec<(&CheckedProgram, &cortexkit_doctor::Finding)> {
        let mut offers = checked
            .iter()
            .flat_map(|program| {
                program.check.findings().iter().filter_map(move |finding| {
                    matches!(finding.fix().class(), FixClass::Safe | FixClass::Risky)
                        .then_some((program, finding))
                })
            })
            .collect::<Vec<_>>();
        offers.sort_unstable_by(|(a, x), (b, y)| {
            (x.fix().class() == FixClass::Risky, &a.module_id, x.id()).cmp(&(
                y.fix().class() == FixClass::Risky,
                &b.module_id,
                y.id(),
            ))
        });
        offers
    }

    pub(in crate::doctor) fn skipped(
        checked: &[CheckedProgram],
        reason: ModuleFixSkip,
    ) -> Vec<ModuleFix> {
        offered(checked)
            .into_iter()
            .map(|(program, finding)| {
                let mut row = fix_row(program, finding);
                row.skip_reason = Some(reason);
                row
            })
            .collect()
    }

    fn fix_row(program: &CheckedProgram, finding: &cortexkit_doctor::Finding) -> ModuleFix {
        ModuleFix {
            module: program.module_id.clone(),
            finding: finding.id().to_string(),
            class: finding.fix().class(),
            decision: ModuleFixDecision::Skipped,
            skip_reason: None,
            result: None,
            restart: None,
        }
    }

    #[derive(Default)]
    pub(in crate::doctor) struct Repairs {
        pub rows: Vec<ModuleFix>,
        pub warnings: Vec<Finding>,
        pub failed: bool,
        pub recheck: bool,
    }

    pub(in crate::doctor) fn repair(
        programs: &Programs<'_>,
        host: &Arc<dyn DoctorHost>,
        daemon: Option<Arc<dyn DoctorDaemon>>,
        rpc_budget: Duration,
        config: &ProgramConfig,
        checked: &[CheckedProgram],
    ) -> Repairs {
        let mut daemon = Daemon {
            host,
            current: daemon,
            rpc_budget,
        };
        let mut repairs = Repairs::default();
        let options = config.runner.clone().with_limits(config.limits);
        for (program, finding) in offered(checked) {
            let mut row = fix_row(program, finding);
            let interrupt = programs.interrupt;
            row.skip_reason = if interrupt.signal().is_some() {
                Some(ModuleFixSkip::Interrupted)
            } else if interrupt.setup_failed() {
                Some(ModuleFixSkip::SignalSetupFailed)
            } else if !programs.prompter.interactive() {
                Some(ModuleFixSkip::NoTerminal)
            } else if !programs.prompter.confirm(&program.module_id, finding) {
                row.decision = ModuleFixDecision::Declined;
                None
            } else {
                attempt(
                    programs,
                    &mut daemon,
                    config.limits,
                    &options,
                    (program, finding),
                    &mut row,
                    &mut repairs.warnings,
                )
            };
            if interrupt.signal().is_some()
                && row.result.is_none()
                && row.decision != ModuleFixDecision::Declined
            {
                row.skip_reason = Some(ModuleFixSkip::Interrupted);
            }
            let outcome = row.result.as_ref().map(|result| result.outcome);
            repairs.recheck |= matches!(
                outcome,
                Some(
                    ModuleFixOutcome::Fixed | ModuleFixOutcome::Failed | ModuleFixOutcome::NoResult
                )
            );
            repairs.failed |= matches!(
                outcome,
                Some(ModuleFixOutcome::Failed | ModuleFixOutcome::NoResult)
            ) || row.restart == Some(ModuleRestart::Failed);
            repairs.rows.push(row);
        }
        repairs
    }

    fn attempt(
        programs: &Programs<'_>,
        daemon: &mut Daemon<'_>,
        limits: runner::Limits,
        options: &runner::Options,
        (program, finding): (&CheckedProgram, &cortexkit_doctor::Finding),
        row: &mut ModuleFix,
        warnings: &mut Vec<Finding>,
    ) -> Option<ModuleFixSkip> {
        if programs.interrupt.signal().is_some() {
            return Some(ModuleFixSkip::Interrupted);
        }
        if !program.unchanged() {
            return Some(ModuleFixSkip::ProgramChanged);
        }

        let id = &program.module_id;
        let fix = finding.fix();
        let plan = fix
            .plan()
            .expect("a validated safe or risky fix has a plan");
        let mut owned = None;
        let mut skip = None;
        if fix.needs_stop() {
            if let Some(entry) = daemon.entry(id).filter(|entry| entry.enabled) {
                let budget = mutation_budget(&entry, programs.operator_budget);
                let deadline = Instant::now() + budget;
                let Some(guard) = programs
                    .interrupt
                    .protect("a module stop or operator approval", deadline)
                else {
                    return Some(ModuleFixSkip::Interrupted);
                };
                let (owns_stop, may_fix) = daemon.stop(id, deadline, &guard, programs);
                if owns_stop {
                    owned = Some((guard, budget));
                }
                if !may_fix {
                    skip = Some(ModuleFixSkip::StopFailed);
                }
            }
        }

        if programs.interrupt.signal().is_some() {
            skip = Some(ModuleFixSkip::Interrupted);
        } else if skip.is_none() {
            let deadline = Instant::now()
                + limits.fix_timeout(plan.timeout_ms())
                + limits.term_to_kill.max(limits.exit_grace);
            if !program.unchanged() {
                skip = Some(ModuleFixSkip::ProgramChanged);
            } else if let Some(_guard) = programs.interrupt.protect("a module fix", deadline) {
                row.decision = ModuleFixDecision::Applied;
                let result = runner::fix(
                    &program.path,
                    finding.id(),
                    plan.digest(),
                    plan.timeout_ms(),
                    options,
                );
                // The runner gives the fix the foreground terminal for its own
                // consent prompt. The terminal's interrupt then reaches the fix,
                // not ck. Latch it here before releasing the guard.
                if let Err(RunError::Crashed(Crash::Signal(signal))) = &result {
                    if *signal == Signal::INT.as_raw() || *signal == Signal::QUIT.as_raw() {
                        programs.interrupt.on_signal(*signal);
                    }
                }
                row.result = Some(fix_result(result));
            } else {
                skip = Some(ModuleFixSkip::Interrupted);
            }
        }

        if let Some((guard, budget)) = owned {
            let deadline = Instant::now() + budget;
            guard.update("a module start or operator approval", deadline);
            row.restart = Some(match daemon.start(id, deadline, &guard) {
                Ok(()) => ModuleRestart::Enabled,
                Err(error) => {
                    warnings.push(diagnostic(
                        "restart_failed",
                        Some(id),
                        Severity::Warning,
                        "Could not restore the module's enabled state",
                        json!({"error": error}),
                    ));
                    ModuleRestart::Failed
                }
            });
        }
        skip
    }

    fn fix_result(
        result: Result<cortexkit_doctor::FixOutput, cortexkit_doctor::RunError>,
    ) -> ModuleFixResult {
        match result {
            Ok(output) => ModuleFixResult {
                outcome: match output.result() {
                    FixResult::Fixed => ModuleFixOutcome::Fixed,
                    FixResult::Refused => ModuleFixOutcome::Refused,
                    FixResult::Failed => ModuleFixOutcome::Failed,
                    _ => ModuleFixOutcome::NoResult,
                },
                code: output.code().map(str::to_owned),
                evidence: output.evidence().map(<[_]>::to_vec),
            },
            Err(error) => ModuleFixResult {
                outcome: ModuleFixOutcome::NoResult,
                code: Some(error.as_str().into()),
                evidence: None,
            },
        }
    }

    fn mutation_budget(entry: &SupervisorEntry, operator: Duration) -> Duration {
        entry
            .drain_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(subc_daemon::DEFAULT_DRAIN_TIMEOUT)
            .saturating_add(DRAIN_RESPONSE_MARGIN)
            .saturating_add(operator)
    }

    struct Daemon<'a> {
        host: &'a Arc<dyn DoctorHost>,
        current: Option<Arc<dyn DoctorDaemon>>,
        rpc_budget: Duration,
    }

    impl Daemon<'_> {
        fn replace(&mut self, guard: Option<&Guard<'_>>, cutoff: Option<Instant>) {
            self.current = None;
            let deadline = Instant::now() + RECONNECT_LIMIT;
            let deadline = cutoff.map_or(deadline, |cutoff| cutoff.min(deadline));
            if let Some(guard) = guard {
                guard.update("the daemon to reconnect", deadline);
            }
            let host = self.host.clone();
            self.current = check_rpc(
                launch(deadline, move || host.describe(None, deadline, &|_| {})).receive(),
            )
            .ok();
        }

        fn list_entry(
            &mut self,
            id: &str,
            deadline: Instant,
            guard: Option<&Guard<'_>>,
        ) -> Option<SupervisorEntry> {
            let result = self.current.as_ref()?.list(deadline);
            if matches!(result, Err(RpcError::ConnectionLost)) {
                self.replace(guard, None);
            }
            result
                .ok()?
                .modules
                .into_iter()
                .find(|entry| entry.module_id == id)
        }

        fn entry(&mut self, id: &str) -> Option<SupervisorEntry> {
            self.list_entry(id, Instant::now() + self.rpc_budget, None)
        }

        fn stop(
            &mut self,
            id: &str,
            deadline: Instant,
            guard: &Guard<'_>,
            programs: &Programs<'_>,
        ) -> (bool, bool) {
            let result = self
                .current
                .as_ref()
                .expect("an enabled entry requires a daemon")
                .set_enabled(id, false, deadline);
            match result {
                Ok(false) => (false, true),
                Ok(true) => (true, self.await_stopped(id, guard, programs)),
                Err(SetEnabledError::Reply(code))
                    if matches!(
                        code.as_str(),
                        "operator_declined" | "operator_presence_unavailable"
                    ) =>
                {
                    (false, false)
                }
                Err(SetEnabledError::Reply(_)) => {
                    let deadline = Instant::now() + self.rpc_budget;
                    guard.update("a module state read", deadline);
                    let untouched = self
                        .list_entry(id, deadline, Some(guard))
                        .is_some_and(|row| row.enabled && row.state != "draining");
                    (!untouched, false)
                }
                Err(SetEnabledError::Rpc(error)) => {
                    // The disable can still be queued. A snapshot of its old state
                    // cannot prove that it is safe to omit the compensating start.
                    if matches!(error, RpcError::ConnectionLost) {
                        self.replace(Some(guard), None);
                    }
                    (true, false)
                }
            }
        }

        fn await_stopped(&mut self, id: &str, guard: &Guard<'_>, programs: &Programs<'_>) -> bool {
            let cutoff = Instant::now() + POLL_LIMIT;
            guard.update("the module to stop", cutoff);
            while self.current.is_some()
                && Instant::now() < cutoff
                && programs.interrupt.signal().is_none()
            {
                let deadline = cutoff.min(Instant::now() + self.rpc_budget);
                if self
                    .list_entry(id, deadline, Some(guard))
                    .is_some_and(|row| {
                        !row.enabled
                            && !row.live
                            && row.waiting_for_previous_pid.is_none()
                            && matches!(row.state.as_str(), "stopped" | "disabled")
                    })
                {
                    return true;
                }
                thread::sleep(POLL_INTERVAL.min(cutoff.saturating_duration_since(Instant::now())));
            }
            false
        }

        fn start(
            &mut self,
            id: &str,
            deadline: Instant,
            guard: &Guard<'_>,
        ) -> Result<(), &'static str> {
            let daemon = self.current.as_ref().ok_or("no_daemon")?;
            let mut result = daemon.set_enabled(id, true, deadline);
            if matches!(result, Err(SetEnabledError::Rpc(RpcError::ConnectionLost))) {
                // Enabling is idempotent: repeat it once even if the old daemon
                // received the first request, without extending the start budget.
                self.replace(Some(guard), Some(deadline));
                let daemon = self.current.as_ref().ok_or("no_daemon")?;
                if Instant::now() >= deadline {
                    return Err("timed_out");
                }
                guard.update("a module start or operator approval", deadline);
                result = daemon.set_enabled(id, true, deadline);
            }
            match result {
                Ok(_) => Ok(()),
                Err(SetEnabledError::Reply(_)) => Err("error_reply"),
                Err(SetEnabledError::Rpc(RpcError::TimedOut)) => Err("timed_out"),
                Err(SetEnabledError::Rpc(RpcError::Failed(_))) => Err("failed"),
                Err(SetEnabledError::Rpc(RpcError::ConnectionLost)) => {
                    self.current = None;
                    Err("connection_lost")
                }
            }
        }
    }
}
