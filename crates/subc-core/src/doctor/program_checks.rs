//! Read the local module configuration and run bounded contract checks.

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use serde_json::json;
use subc_control::ModuleProtocol;
use subc_daemon::daemon_config::{self, DaemonConfigError};

use super::{Finding, Interrupt, Layer, ModuleCheck, ModuleOutcome, Severity};
use cortexkit_doctor::Mode;

#[cfg(unix)]
use {
    super::{program_fixes::Daemon, ModuleCheckMode, ModuleFixSkip, Programs},
    cortexkit_doctor::{runner, RunError},
    std::{collections::BTreeSet, sync::mpsc, thread, time::Duration},
};

/// A program and its validated check document, bound to the observed file.
#[derive(Debug)]
pub struct CheckedProgram {
    pub module_id: String,
    pub path: PathBuf,
    pub identity: subc_os::FileIdentity,
    pub check: cortexkit_doctor::CheckOutput,
    pub mode: Mode,
}

impl CheckedProgram {
    /// A point-in-time comparison, not a guarantee about a later path-based launch.
    pub fn unchanged(&self) -> bool {
        subc_os::file_identity(&self.path) == Some(self.identity)
    }
}

pub(super) struct ProgramConfig {
    pub path: PathBuf,
    #[cfg(unix)]
    pub runner: cortexkit_doctor::runner::Options,
    #[cfg(unix)]
    pub limits: cortexkit_doctor::runner::Limits,
    #[cfg(all(test, unix))]
    pub admission: std::time::Duration,
    #[cfg(all(test, unix))]
    pub identity_lookup: fn(&Path) -> Option<subc_os::FileIdentity>,
}

impl Default for ProgramConfig {
    fn default() -> Self {
        Self {
            path: daemon_config::default_config_path(),
            #[cfg(unix)]
            runner: cortexkit_doctor::runner::Options::new(),
            #[cfg(unix)]
            limits: cortexkit_doctor::runner::Limits::default(),
            #[cfg(all(test, unix))]
            admission: std::time::Duration::from_secs(11),
            #[cfg(all(test, unix))]
            identity_lookup: subc_os::file_identity,
        }
    }
}

#[derive(Default)]
pub(super) struct ProgramChecks {
    pub modules: Vec<ModuleCheck>,
    pub findings: Vec<Finding>,
    pub checked: Vec<CheckedProgram>,
}

pub(super) fn diagnostic(
    id: &str,
    owner: Option<&str>,
    severity: Severity,
    summary: &str,
    evidence: serde_json::Value,
) -> Finding {
    let finding = Finding::new(
        format!("module_program.{id}"),
        Layer::ModuleProgram,
        severity,
        summary,
        evidence,
    );
    match owner {
        Some(owner) => finding.for_module(owner),
        None => finding,
    }
}

pub(super) fn check_all(
    config: &ProgramConfig,
    override_path: Option<&Path>,
    interrupt: &Interrupt,
    start: Instant,
    #[cfg(unix)] full: Option<FullChecks<'_>>,
) -> ProgramChecks {
    let mut pass = ProgramChecks::default();
    if override_path.is_some() {
        pass.findings.push(diagnostic(
            "connection_override",
            None,
            Severity::Info,
            "Module program checks are not run with a connection override",
            json!({}),
        ));
        return pass;
    }
    let modules = match daemon_config::load(&config.path) {
        Ok(Some(config)) => config.modules,
        Ok(None) => return pass,
        Err(error) => {
            let kind = match error {
                DaemonConfigError::Read { .. } => "read",
                DaemonConfigError::InvalidJsonc { .. } => "invalid_jsonc",
                DaemonConfigError::InvalidJson { .. } => "invalid_json",
                DaemonConfigError::UnsupportedVersion { .. } => "unsupported_version",
                DaemonConfigError::InvalidValue { .. } => "invalid_value",
            };
            pass.findings.push(diagnostic(
                "config_unreadable",
                None,
                Severity::Warning,
                "Module program configuration could not be read",
                json!({"error": kind}),
            ));
            return pass;
        }
    };
    let mut jobs = Vec::new();
    for module in modules {
        let detail = if module.script.is_some() {
            Some("script")
        } else if module.protocol == ModuleProtocol::None {
            Some("protocol_none")
        } else if cfg!(unix) && !module.program.is_absolute() {
            // A relative path could resolve to a different program via PATH or the working directory.
            Some("program_not_absolute")
        } else {
            None
        };
        if let Some(detail) = detail {
            pass.modules.push(row(
                module.module_id,
                ModuleOutcome::NotAsked,
                Some(detail.into()),
            ));
        } else {
            jobs.push((module.module_id, module.program));
        }
    }
    jobs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    #[cfg(unix)]
    {
        #[cfg(test)]
        let cutoff = start + config.admission;
        #[cfg(not(test))]
        let cutoff = start + std::time::Duration::from_secs(11);
        #[cfg(test)]
        let identity_lookup = config.identity_lookup;
        #[cfg(not(test))]
        let identity_lookup = subc_os::file_identity;
        let options = config.runner.clone().with_limits(config.limits);
        if let Some(mut full) = full {
            for (id, path) in jobs {
                let (job, mode, fallback) = full.check(
                    &id,
                    &path,
                    &options,
                    config.limits,
                    identity_lookup,
                    &mut pass.findings,
                );
                let mode = job.ran().then_some(mode);
                record(&mut pass, id, path, job, mode, fallback);
            }
        } else {
            for (id, path, result) in pool_until(
                jobs,
                cutoff,
                |(id, path)| {
                    let result = check_one(
                        &path,
                        interrupt,
                        &options,
                        config.limits,
                        Some(cutoff),
                        identity_lookup,
                    );
                    (id, path, result)
                },
                |(id, path)| (id, path, Job::NotStarted),
            ) {
                record(&mut pass, id, path, result, None, false);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (interrupt, start);
        if !jobs.is_empty() {
            pass.findings.push(diagnostic(
                "unsupported_platform",
                None,
                Severity::Info,
                "Module program checks are not supported on this platform",
                json!({}),
            ));
        }
        pass.modules.extend(
            jobs.into_iter()
                .map(|(id, _)| row(id, ModuleOutcome::UnsupportedPlatform, None)),
        );
    }
    pass.modules.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    pass
}

fn row(id: String, outcome: ModuleOutcome, detail: Option<String>) -> ModuleCheck {
    ModuleCheck {
        id,
        outcome,
        findings: 0,
        detail,
        mode: None,
    }
}

/// Run ordered program jobs in a pool of four with eleven seconds to admit work.
/// Each job owns its execution deadline; a slot is not freed until cleanup ends.
/// The returned values preserve input order.
#[cfg(unix)]
pub fn run_program_pool<T: Send, R: Send>(
    jobs: Vec<T>,
    run: impl Fn(T) -> R + Sync,
    not_started: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    pool_until(
        jobs,
        Instant::now() + std::time::Duration::from_secs(11),
        run,
        not_started,
    )
}

#[cfg(unix)]
fn pool_until<T: Send, R: Send>(
    jobs: Vec<T>,
    cutoff: Instant,
    run: impl Fn(T) -> R + Sync,
    not_started: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    use std::{sync::Mutex, thread};

    let width = jobs.len().min(4);
    let queue = Mutex::new(jobs.into_iter().enumerate());
    let mut results = thread::scope(|scope| {
        let workers = (0..width)
            .map(|_| {
                scope.spawn(|| {
                    let mut results = Vec::new();
                    loop {
                        let job = {
                            let mut queue = queue.lock().unwrap();
                            if Instant::now() > cutoff {
                                break;
                            }
                            queue.next()
                        };
                        let Some((i, job)) = job else { break };
                        results.push((i, run(job)));
                    }
                    results
                })
            })
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("program worker panicked"))
            .collect::<Vec<_>>()
    });
    results.extend(
        queue
            .into_inner()
            .unwrap()
            .map(|(i, job)| (i, not_started(job))),
    );
    results.sort_unstable_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, result)| result).collect()
}

#[cfg(unix)]
enum Job {
    Checked(subc_os::FileIdentity, cortexkit_doctor::CheckOutput),
    IdentityUnavailable,
    Failed(cortexkit_doctor::RunError),
    NotStarted,
}

#[cfg(unix)]
impl Job {
    fn ran(&self) -> bool {
        !matches!(
            self,
            Self::NotStarted | Self::Failed(RunError::LaunchFailed(_) | RunError::HomeRelative)
        )
    }

    fn from_result(
        identity: Option<subc_os::FileIdentity>,
        result: Result<cortexkit_doctor::CheckOutput, RunError>,
    ) -> Self {
        match (identity, result) {
            (Some(identity), Ok(output)) => Self::Checked(identity, output),
            (None, Ok(_)) => Self::IdentityUnavailable,
            (_, Err(error)) => Self::Failed(error),
        }
    }
}

#[cfg(unix)]
fn check_one(
    path: &Path,
    interrupt: &Interrupt,
    options: &cortexkit_doctor::runner::Options,
    limits: cortexkit_doctor::runner::Limits,
    cutoff: Option<Instant>,
    identity_lookup: impl FnOnce(&Path) -> Option<subc_os::FileIdentity>,
) -> Job {
    if interrupt.signal().is_some() {
        return Job::NotStarted;
    }
    // Keep the pre-launch identity so repairs and issue reports can detect a
    // replaced program. The runner, not the identity lookup, diagnoses launch failures.
    let identity = identity_lookup(path);
    if cutoff.is_some_and(|cutoff| Instant::now() > cutoff) {
        return Job::NotStarted;
    }
    let Some(_guard) = interrupt.protect(
        "a module check",
        Instant::now() + limits.check + limits.term_to_kill.max(limits.exit_grace),
    ) else {
        return Job::NotStarted;
    };
    Job::from_result(identity, runner::check(path, Mode::Quick, options))
}

#[cfg(unix)]
pub(super) struct FullChecks<'a> {
    pub programs: &'a Programs<'a>,
    pub daemon: Daemon<'a>,
    pub asked: &'a mut BTreeSet<String>,
}

#[cfg(unix)]
impl FullChecks<'_> {
    fn check(
        &mut self,
        id: &str,
        path: &Path,
        options: &runner::Options,
        limits: runner::Limits,
        identity_lookup: fn(&Path) -> Option<subc_os::FileIdentity>,
        warnings: &mut Vec<Finding>,
    ) -> (Job, Mode, bool) {
        let programs = self.programs;
        let interrupt = programs.interrupt;
        let identity = identity_lookup(path);
        let deadline =
            || Instant::now() + limits.full_check + limits.term_to_kill.max(limits.exit_grace);
        let mut job = if let Some(_guard) = interrupt.protect("a full module check", deadline()) {
            Job::from_result(
                identity,
                wait_for_full_check(programs, id, path, options, limits.full_check),
            )
        } else {
            Job::NotStarted
        };
        let store_in_use = matches!(&job, Job::Checked(_, check) if check.findings().iter().any(|finding| finding.id().as_str() == "store.in_use"));
        if store_in_use
            && programs.full_feedback
            && programs.prompter.interactive()
            && !interrupt.setup_failed()
            && interrupt.signal().is_none()
            && !self.asked.contains(id)
        {
            if let Some(entry) = self
                .daemon
                .entry(id)
                .filter(|entry| entry.enabled && entry.live)
            {
                self.asked.insert(id.to_owned());
                if programs.prompter.confirm_deep_stop(id) && identity_lookup(path) == identity {
                    let (retry, _) =
                        self.daemon
                            .with_stopped(programs, id, Some(&entry), warnings, |owned| {
                                if identity_lookup(path) != identity {
                                    return Err(ModuleFixSkip::ProgramChanged);
                                }
                                let _guard;
                                if let Some(guard) = owned {
                                    guard.wait_for_stopped_check(deadline());
                                } else {
                                    _guard = interrupt
                                        .protect("a full module check", deadline())
                                        .ok_or(ModuleFixSkip::Interrupted)?;
                                }
                                if interrupt.signal().is_some() {
                                    return Err(ModuleFixSkip::Interrupted);
                                }
                                Ok(Job::from_result(
                                    identity,
                                    wait_for_full_check(
                                        programs,
                                        id,
                                        path,
                                        options,
                                        limits.full_check,
                                    ),
                                ))
                            });
                    if let Ok(retry) = retry {
                        job = retry;
                    }
                }
            }
        }
        if matches!(job, Job::Failed(RunError::NoDoctor)) && interrupt.signal().is_none() {
            let quick = check_one(path, interrupt, options, limits, None, identity_lookup);
            if !matches!(quick, Job::NotStarted) {
                return (quick, Mode::Quick, true);
            }
        }
        (job, Mode::Full, false)
    }
}

#[cfg(unix)]
fn wait_for_full_check(
    programs: &Programs<'_>,
    id: &str,
    path: &Path,
    options: &runner::Options,
    limit: Duration,
) -> Result<cortexkit_doctor::CheckOutput, RunError> {
    if !programs.full_feedback {
        return runner::check(path, Mode::Full, options);
    }
    // Only progress output runs alongside the check. Wait for the runner to
    // clean up the check process before starting the next operation.
    let start = Instant::now();
    thread::scope(|scope| {
        let (send, receive) = mpsc::sync_channel(1);
        scope.spawn(move || {
            let _ = send.send(runner::check(path, Mode::Full, options));
        });
        loop {
            if programs.interrupt.signal().is_none() {
                programs
                    .prompter
                    .full_progress(id, start.elapsed().min(limit), limit);
            }
            match receive.recv_timeout(Duration::from_secs(1)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("full check worker panicked"),
            }
        }
    })
}

#[cfg(unix)]
fn io_kind(kind: std::io::ErrorKind) -> &'static str {
    use std::io::ErrorKind;
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::NotADirectory => "not_a_directory",
        ErrorKind::InvalidInput => "invalid_input",
        _ => "other",
    }
}

#[cfg(unix)]
fn record(
    pass: &mut ProgramChecks,
    id: String,
    path: PathBuf,
    job: Job,
    mode: Option<Mode>,
    fallback: bool,
) {
    use cortexkit_doctor::{Crash, DecodeError};
    let reported_mode = mode.map(|mode| match mode {
        Mode::Quick => ModuleCheckMode::Quick,
        Mode::Full => ModuleCheckMode::Full,
    });
    let detail_with_mode = |detail: Option<String>| match (fallback, detail) {
        (true, Some(detail)) => Some(format!("full_not_supported; {detail}")),
        (true, None) => Some("full_not_supported".into()),
        (false, detail) => detail,
    };

    let (outcome, detail, diagnostic_kind) = match job {
        Job::Checked(identity, check) => {
            let findings = check.findings();
            let outcome = if findings.is_empty() {
                ModuleOutcome::Clean
            } else {
                ModuleOutcome::Findings
            };
            for finding in findings {
                let severity = match finding.severity() {
                    cortexkit_doctor::Severity::Error => Severity::Error,
                    cortexkit_doctor::Severity::Warning => Severity::Warning,
                    _ => Severity::Info,
                };
                let mut row = Finding::new(
                    finding.id().as_str(),
                    Layer::ModuleProgram,
                    severity,
                    finding.summary(),
                    finding.evidence(),
                )
                .for_module(&id);
                row.fix = Some(finding.fix().clone());
                pass.findings.push(row);
            }
            pass.modules.push(ModuleCheck {
                id: id.clone(),
                outcome,
                findings: findings.len(),
                detail: detail_with_mode(None),
                mode: reported_mode,
            });
            pass.checked.push(CheckedProgram {
                module_id: id,
                path,
                identity,
                check,
                mode: mode.unwrap_or(Mode::Quick),
            });
            return;
        }
        Job::Failed(RunError::NoDoctor) => (ModuleOutcome::NoChecks, None, None),
        // The child ran, but output we can't tie to a file identity is unusable.
        Job::IdentityUnavailable => (
            ModuleOutcome::InvalidOutput,
            Some("identity_unreadable".into()),
            Some((
                "identity_unreadable",
                "The module program's file identity could not be read, so its results can't be trusted for repairs",
            )),
        ),
        Job::Failed(RunError::LaunchFailed(kind)) => (
            ModuleOutcome::SpawnFailed,
            Some(io_kind(kind).into()),
            Some(("spawn_failed", "The module check could not launch")),
        ),
        Job::Failed(RunError::Crashed(status)) => (
            ModuleOutcome::Crashed,
            Some(match status {
                Crash::ExitCode(code) => format!("exit:{code}"),
                Crash::Signal(signal) => format!("signal:{signal}"),
            }),
            Some(("crashed", "The module check exited unsuccessfully")),
        ),
        Job::Failed(RunError::TimedOut) => (
            ModuleOutcome::TimedOut,
            None,
            Some(("timed_out", "The module check exceeded its deadline")),
        ),
        Job::Failed(RunError::InvalidOutput(error)) => (
            ModuleOutcome::InvalidOutput,
            Some(
                if error == DecodeError::TooLarge {
                    "oversize"
                } else {
                    error.as_str()
                }
                .into(),
            ),
            Some((
                "invalid_output",
                "The module check did not return a valid document",
            )),
        ),
        Job::Failed(error) => (
            ModuleOutcome::SpawnFailed,
            Some(error.as_str().into()),
            Some(("spawn_failed", "The module check could not launch")),
        ),
        Job::NotStarted => (
            ModuleOutcome::NotStarted,
            None,
            Some(("not_started", "The module check was not started")),
        ),
    };
    if let Some((kind, summary)) = diagnostic_kind {
        pass.findings.push(diagnostic(
            kind,
            Some(&id),
            Severity::Warning,
            summary,
            json!({"detail": detail}),
        ));
    }
    pass.modules.push(ModuleCheck {
        mode: reported_mode,
        ..row(id, outcome, detail_with_mode(detail))
    });
}
