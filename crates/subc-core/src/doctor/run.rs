use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cortexkit_doctor::Mode;
use serde_json::json;

use crate::stack::MarkerState;

use super::{
    classify_service_and_daemon, fix,
    host::{DoctorDaemon, RpcError, RpcResult, SystemHost},
    module_checks::{self, ModuleFindings, OfflineLogs, HEALTH, LIST},
    program_checks::{self, CheckedProgram, ProgramChecks, ProgramConfig},
    program_fixes::Programs,
    report::{DoctorReport, DoctorRun, Finding, Layer, LogTailRecord, Offer, Severity},
    stack_checks::{self, SignatureProbes},
    window_checks::{self, WindowFindings},
    DescribeOutcome, DoctorHost, DoctorOptions, LogFallback,
};

/// Run one diagnostic pass and, when requested, attempt the offered window revert.
///
/// This is synchronous. Call it outside an async executor; the checks own their
/// deadlines and a revert can wait for operating-system approval.
pub fn run(options: DoctorOptions, logs: Arc<dyn LogFallback>) -> DoctorRun {
    run_with(
        Arc::new(SystemHost::default()),
        effective_options(options),
        logs,
        &Limits::default(),
    )
}

fn effective_options(mut options: DoctorOptions) -> DoctorOptions {
    options.connection_override = options.connection_override.or_else(|| {
        std::env::var_os("SUBC_CONNECTION_FILE")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    });
    options
}

/// The final report and the validated program checks that contributed to it.
#[derive(Debug)]
pub struct ProgramRun {
    pub run: DoctorRun,
    pub checked: Vec<CheckedProgram>,
}

/// Run the stack doctor and check local module programs. When requested, apply
/// consented fixes or the existing window revert, but not both in one run.
/// This is synchronous; the caller owns terminal dialogue and signal delivery.
pub fn run_with_programs(
    options: DoctorOptions,
    programs: Programs<'_>,
    logs: Arc<dyn LogFallback>,
) -> ProgramRun {
    let options = effective_options(options);
    let host = SystemHost::default();
    let host = if options.fix || (programs.mode == Mode::Full && programs.full_feedback) {
        // The SDK's connection-wide timeout must not cut off a long drain or
        // operator prompt. The host still bounds each call by its own deadline.
        let call_timeout = Duration::from_millis(u64::MAX)
            .saturating_add(super::program_fixes::DRAIN_RESPONSE_MARGIN)
            .saturating_add(programs.operator_budget);
        SystemHost { call_timeout }
    } else {
        host
    };
    run_with_programs_on(
        Arc::new(host),
        options,
        programs,
        logs,
        &Limits::default(),
        &ProgramConfig::default(),
    )
}

pub(super) struct Limits {
    pub(super) describe: Duration,
    pub(super) rpc: Duration,
    pub(super) window: Duration,
    pub(super) local: Duration,
    pub(super) modules: Duration,
    #[cfg(test)]
    pub(super) budget_override: Option<(Budget, Duration)>,
    #[cfg(test)]
    pub(super) before_describe_launch: Option<fn(Instant)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Budget {
    Manager,
    Cpu,
    List,
}

impl Limits {
    fn budget(&self, stage: Budget) -> Duration {
        #[cfg(test)]
        if let Some((overridden, duration)) = self.budget_override {
            if overridden == stage {
                return duration;
            }
        }
        match stage {
            Budget::Manager | Budget::Cpu => self.local,
            Budget::List => self.rpc,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            describe: Duration::from_secs(2),
            rpc: Duration::from_secs(3),
            window: Duration::from_secs(3),
            local: Duration::from_secs(5),
            modules: Duration::from_secs(15),
            #[cfg(test)]
            budget_override: None,
            #[cfg(test)]
            before_describe_launch: None,
        }
    }
}

pub(super) fn run_with(
    host: Arc<dyn DoctorHost>,
    options: DoctorOptions,
    logs: Arc<dyn LogFallback>,
    limits: &Limits,
) -> DoctorRun {
    let connection_override = options.connection_override;
    if options.fix && connection_override.is_some() {
        return Pass::default().report(connection_override, None, Some(2));
    }
    let check = || pass(&host, &logs, connection_override.clone(), limits, false);
    let first = check();
    let (final_pass, fix, exit) = if options.fix {
        let (pass, result, exit) = fix::attempt(host.as_ref(), first, check);
        (pass, Some(result), exit)
    } else {
        (first, None, None)
    };
    final_pass.report(connection_override, fix, exit)
}

pub(super) fn run_with_programs_on(
    host: Arc<dyn DoctorHost>,
    options: DoctorOptions,
    programs: Programs<'_>,
    logs: Arc<dyn LogFallback>,
    limits: &Limits,
    config: &ProgramConfig,
) -> ProgramRun {
    let path = options.connection_override;
    if options.fix && path.is_some() {
        let pass = Pass {
            programs: Some(ProgramChecks::default()),
            ..Pass::default()
        };
        return pass.program_report(path, None, Some(2));
    }
    let asked = std::cell::RefCell::new(std::collections::BTreeSet::new());
    let check = || {
        if programs.mode == Mode::Full {
            let mut pass = pass(&host, &logs, path.clone(), limits, true);
            let checks = program_checks::check_all(
                config,
                path.as_deref(),
                programs.interrupt,
                Instant::now(),
                Some(program_checks::FullChecks {
                    programs: &programs,
                    daemon: super::program_fixes::Daemon::new(
                        &host,
                        pass.daemon.clone(),
                        limits.rpc,
                    ),
                    asked: &mut asked.borrow_mut(),
                }),
            );
            pass.add_program_checks(checks);
            return pass;
        }
        thread::scope(|scope| {
            let start = Instant::now();
            let override_path = path.as_deref();
            let interrupt = programs.interrupt;
            let checks = scope.spawn(move || {
                program_checks::check_all(config, override_path, interrupt, start, None)
            });
            let mut pass = pass(&host, &logs, path.clone(), limits, options.fix);
            pass.add_program_checks(checks.join().expect("program check worker panicked"));
            pass
        })
    };
    let first = check();
    if !options.fix {
        return first.program_report(path, None, None);
    }
    let (last, fix, exit) = fix_program_pass(&host, first, check, &programs, limits, config);
    last.program_report(path, Some(fix), exit)
}

fn fix_program_pass(
    host: &Arc<dyn DoctorHost>,
    first: Pass,
    check: impl FnOnce() -> Pass,
    programs: &Programs<'_>,
    limits: &Limits,
    config: &ProgramConfig,
) -> (Pass, super::FixReport, Option<u8>) {
    use super::{FixReport, FixSkip};

    if programs.interrupt.signal().is_some() {
        let report = FixReport {
            result: None,
            skipped: Some(FixSkip::Interrupted),
            findings_before: first.findings.clone(),
            module_fixes: super::program_fixes::skipped(
                first.checked(),
                super::ModuleFixSkip::Interrupted,
            ),
        };
        return (first, report, None);
    }
    {
        use super::{program_fixes, ModuleFixSkip};

        if first.offer.is_some() {
            let reason = if first.manager_overdue {
                ModuleFixSkip::ManagerObservationOverdue
            } else {
                ModuleFixSkip::RevertFirst
            };
            let rows = program_fixes::skipped(first.checked(), reason);
            let (last, mut report, exit) = fix::attempt(host.as_ref(), first, check);
            report.module_fixes = rows;
            return (last, report, exit);
        }
        let mut first = first;
        let daemon = first.daemon.take();
        let repairs =
            program_fixes::repair(programs, host, daemon, limits.rpc, config, first.checked());
        let report = FixReport {
            result: None,
            skipped: Some(FixSkip::NothingToFix),
            findings_before: first.findings.clone(),
            module_fixes: repairs.rows,
        };
        let mut last = if repairs.recheck && programs.interrupt.signal().is_none() {
            check()
        } else {
            first
        };
        last.findings.extend(repairs.warnings);
        Finding::sort(&mut last.findings);
        (last, report, repairs.failed.then_some(1))
    }
}

#[derive(Default)]
pub(super) struct Pass {
    pub(super) findings: Vec<Finding>,
    pub(super) offer: Option<Offer>,
    pub(super) manager_overdue: bool,
    daemon_answered: bool,
    tails: Vec<LogTailRecord>,
    programs: Option<ProgramChecks>,
    daemon: Option<Arc<dyn DoctorDaemon>>,
}

impl Pass {
    fn add_program_checks(&mut self, mut checks: ProgramChecks) {
        self.findings.append(&mut checks.findings);
        Finding::sort(&mut self.findings);
        self.programs = Some(checks);
    }

    fn checked(&self) -> &[CheckedProgram] {
        self.programs
            .as_ref()
            .map_or(&[], |programs| &programs.checked)
    }

    fn program_report(
        mut self,
        path: Option<PathBuf>,
        fix: Option<super::FixReport>,
        exit: Option<u8>,
    ) -> ProgramRun {
        let checked = self
            .programs
            .as_mut()
            .map_or_else(Vec::new, |programs| std::mem::take(&mut programs.checked));
        ProgramRun {
            run: self.report(path, fix, exit),
            checked,
        }
    }

    pub(super) fn report(
        self,
        connection_override: Option<PathBuf>,
        fix: Option<super::FixReport>,
        exit: Option<u8>,
    ) -> DoctorRun {
        let ok = !self
            .findings
            .iter()
            .any(|finding| finding.severity == Severity::Error);
        DoctorRun {
            document: DoctorReport {
                schema: "ck.doctor.report/v1",
                ok,
                daemon_answered: self.daemon_answered,
                connection_override,
                findings: self.findings,
                offer: self.offer,
                report: None,
                fix,
                modules: self.programs.map(|programs| programs.modules),
            },
            log_tails: self.tails,
            exit_code: exit.unwrap_or(u8::from(!ok)),
        }
    }
}

fn pass(
    host: &Arc<dyn DoctorHost>,
    logs: &Arc<dyn LogFallback>,
    connection_override: Option<PathBuf>,
    limits: &Limits,
    keep_daemon: bool,
) -> Pass {
    let start = Instant::now();
    let local_deadline = start + limits.local;
    let probes = SignatureProbes::default();
    let install = launch(local_deadline, {
        let (host, probes) = (host.clone(), probes.clone());
        move || stack_checks::install(host.as_ref(), local_deadline, probes)
    });
    let disk = launch(local_deadline, {
        let host = host.clone();
        move || stack_checks::disk(host.as_ref())
    });
    let service = launch(start + limits.budget(Budget::Manager), {
        let host = host.clone();
        let use_process = connection_override.is_none();
        move || {
            let (manager, marker) = host.manager();
            let process = if use_process {
                manager
                    .as_ref()
                    .ok()
                    .and_then(|job| job.pid)
                    .and_then(|pid| host.process_entry(pid))
            } else {
                None
            };
            (manager, marker, process)
        }
    });
    let windows = launch(start + limits.window, {
        let host = host.clone();
        move || host.windows()
    });

    let hint = Arc::new(AtomicU64::new(0));
    let describe_deadline = start + limits.describe;
    #[cfg(test)]
    if let Some(before_launch) = limits.before_describe_launch {
        before_launch(start);
    }
    let describe = launch(describe_deadline, {
        let (host, connection_override, hint) =
            (host.clone(), connection_override.clone(), hint.clone());
        move || {
            host.describe(connection_override.as_deref(), describe_deadline, &|pid| {
                if Instant::now() < describe_deadline {
                    hint.store(u64::from(pid) + 1, Ordering::Release);
                }
            })
        }
    });
    let daemon = check_rpc(describe.receive()).ok();
    let describe = DescribeOutcome {
        answered: daemon.is_some(),
        connection_pid_hint: hint
            .load(Ordering::Acquire)
            .checked_sub(1)
            .map(|pid| pid as u32),
        override_path: connection_override,
    };
    let fix_daemon = keep_daemon.then(|| daemon.clone()).flatten();

    let mut findings = Vec::new();
    let (
        daemon_findings,
        ModuleFindings {
            findings: module_findings,
            tails,
            lost_at,
        },
    ) = if let Some(daemon) = daemon {
        // Send the daemon its module-list and health requests now, so they are
        // in flight while the local checks below run.
        online(host.clone(), daemon, start + limits.modules, limits)
    } else {
        let logs = describe
            .override_path
            .is_none()
            .then(|| OfflineLogs::start(host.clone(), logs.clone(), limits.local));
        (
            stack_checks::resources(Err("daemon did not answer")),
            module_checks::offline(logs),
        )
    };
    findings.extend(daemon_findings);
    findings.extend(module_findings);
    if let Some(step) = lost_at {
        findings.push(Finding::new(
            "daemon.connection_lost",
            Layer::Daemon,
            Severity::Error,
            format!(
                "The daemon connection was lost during {step}; the pass did not use a replacement"
            ),
            json!({"step": step}),
        ));
    }

    let install_result = install.receive();
    probes.close();
    if !matches!(&install_result, Check::Finished(rows) if rows.iter().any(|row| row.id == "install.versions"))
    {
        findings.push(stack_checks::versions_finding(&Default::default()));
    }
    findings.extend(local_findings(install_result, Layer::Install, "install"));
    findings.extend(local_findings(disk.receive(), Layer::Disk, "disk"));

    let WindowFindings {
        findings: window_findings,
        offer,
        blocker,
    } = match windows.receive() {
        Check::Finished(rows) => window_checks::classify(rows, now_ms()),
        check => WindowFindings {
            findings: local_findings(check.map(|_| Vec::new()), Layer::Window, "window read"),
            ..WindowFindings::default()
        },
    };
    findings.extend(window_findings);

    let mut manager_overdue = false;
    let mut service_findings = match service.receive() {
        Check::Finished((manager, marker, process)) => {
            classify_service_and_daemon(manager, marker, &describe, process)
        }
        Check::TimedOut => {
            manager_overdue = true;
            let mut unknown = classify_service_and_daemon(
                Err("manager observation timed out".into()),
                MarkerState::Unknown,
                &describe,
                None,
            );
            unknown.retain(|finding| finding.layer == Layer::Daemon);
            unknown.push(timeout(Layer::Service, "manager observation"));
            unknown
        }
        Check::Failed => classify_service_and_daemon(
            Err("manager observation worker did not finish".into()),
            MarkerState::Unknown,
            &describe,
            None,
        ),
    };
    stack_checks::advise_start(
        &mut service_findings,
        blocker
            .as_ref()
            .map(|(id, subject)| (*id, subject.as_str())),
    );
    findings.extend(service_findings);
    Finding::sort(&mut findings);
    Pass {
        findings,
        offer: offer.filter(|_| describe.override_path.is_none()),
        manager_overdue,
        daemon_answered: describe.answered,
        tails,
        programs: None,
        daemon: fix_daemon,
    }
}

fn online(
    host: Arc<dyn DoctorHost>,
    daemon: Arc<dyn DoctorDaemon>,
    cutoff: Instant,
    limits: &Limits,
) -> (Vec<Finding>, ModuleFindings) {
    let list = launch_rpc(limits.budget(Budget::List), {
        let daemon = daemon.clone();
        move |deadline| daemon.list(deadline)
    });
    let health = launch_rpc(limits.rpc, {
        let daemon = daemon.clone();
        move |deadline| daemon.health(deadline)
    });

    let mut module_findings = Vec::new();
    let mut first_loss = None;
    let mut cpu = None;
    let mut unmeasured = "no resource row";
    let modules = match check_rpc(list.receive()) {
        Ok(mut list) => {
            if let Some(row) = list.daemon {
                cpu = Some(launch(
                    Instant::now() + limits.budget(Budget::Cpu),
                    move || stack_checks::sample_cpu(host.as_ref(), row.pid, row.start_time_ms),
                ));
            }
            list.modules.sort_by(|a, b| a.module_id.cmp(&b.module_id));
            list.modules
        }
        Err(error) => {
            unmeasured = match &error {
                RpcError::TimedOut => "supervisor.list timed out",
                RpcError::Failed(_) => "supervisor.list failed",
                RpcError::ConnectionLost => {
                    first_loss = Some(LIST);
                    "daemon connection lost"
                }
            };
            module_findings.push(module_checks::rpc_finding(LIST, None, error));
            Vec::new()
        }
    };
    let reads = module_checks::collect_modules(daemon, &modules, cutoff, limits.rpc);
    let health = match check_rpc(health.receive()) {
        Ok(health) => health,
        Err(error) => {
            if matches!(error, RpcError::ConnectionLost) {
                first_loss.get_or_insert(HEALTH);
            }
            module_findings.push(module_checks::rpc_finding(HEALTH, None, error));
            Vec::new()
        }
    };
    let mut module = reads.findings(&modules, &health, now_ms());
    module.lost_at = first_loss.or(module.lost_at);
    module.findings.extend(module_findings);
    (cpu_findings(cpu, unmeasured), module)
}

fn cpu_findings(
    cpu: Option<Pending<Result<stack_checks::CpuSample, &'static str>>>,
    absent: &str,
) -> Vec<Finding> {
    match cpu.map(Pending::receive) {
        None => stack_checks::resources(Err(absent)),
        Some(Check::Finished(sample)) => stack_checks::resources(sample),
        Some(Check::TimedOut) => {
            let mut findings = stack_checks::resources(Err("CPU sample timed out"));
            findings.push(timeout(Layer::Daemon, "CPU sample"));
            findings
        }
        Some(Check::Failed) => {
            let mut findings = stack_checks::resources(Err("CPU sample failed"));
            findings.push(Finding::check(
                Layer::Daemon,
                Severity::Warning,
                false,
                "CPU sample",
                None,
                "The check worker did not finish",
            ));
            findings
        }
    }
}

pub(super) fn check_rpc<T>(result: Check<RpcResult<T>>) -> RpcResult<T> {
    match result {
        Check::Finished(result) => result,
        Check::TimedOut => Err(RpcError::TimedOut),
        Check::Failed => Err(RpcError::Failed("The check worker did not finish".into())),
    }
}

fn local_findings(check: Check<Vec<Finding>>, layer: Layer, name: &'static str) -> Vec<Finding> {
    match check {
        Check::Finished(findings) => findings,
        Check::TimedOut => vec![timeout(layer, name)],
        Check::Failed => vec![Finding::check(
            layer,
            Severity::Warning,
            false,
            name,
            None,
            "The check worker did not finish",
        )],
    }
}

fn timeout(layer: Layer, name: &'static str) -> Finding {
    Finding::check(
        layer,
        Severity::Error,
        true,
        name,
        None,
        "The check did not finish before its deadline",
    )
}

pub(super) enum Check<T> {
    Finished(T),
    TimedOut,
    Failed,
}

impl<T> Check<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Check<U> {
        match self {
            Self::Finished(value) => Check::Finished(f(value)),
            Self::TimedOut => Check::TimedOut,
            Self::Failed => Check::Failed,
        }
    }
}

pub(super) struct Pending<T> {
    receiver: mpsc::Receiver<(Instant, T)>,
    deadline: Instant,
}

impl<T> Pending<T> {
    pub(super) fn receive(self) -> Check<T> {
        match self
            .receiver
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
        {
            Ok((finished, result)) if finished <= self.deadline => Check::Finished(result),
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => Check::TimedOut,
            Err(mpsc::RecvTimeoutError::Disconnected) if Instant::now() >= self.deadline => {
                Check::TimedOut
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Check::Failed,
        }
    }
}

pub(super) fn launch<T: Send + 'static>(
    deadline: Instant,
    check: impl FnOnce() -> T + Send + 'static,
) -> Pending<T> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        if Instant::now() < deadline {
            let result = check();
            let _ = sender.send((Instant::now(), result));
        }
    });
    Pending { receiver, deadline }
}

fn launch_rpc<T: Send + 'static>(
    budget: Duration,
    rpc: impl FnOnce(Instant) -> RpcResult<T> + Send + 'static,
) -> Pending<RpcResult<T>> {
    let deadline = Instant::now() + budget;
    launch(deadline, move || rpc(deadline))
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
