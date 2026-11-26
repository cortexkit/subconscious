//! Read the local module configuration and run bounded contract checks.

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use serde_json::json;
use subc_control::ModuleProtocol;
use subc_daemon::daemon_config::{self, DaemonConfigError};

use super::{Finding, Interrupt, Layer, ModuleCheck, ModuleOutcome, Severity};

/// A program and its validated check document, bound to the observed file.
#[derive(Debug)]
pub struct CheckedProgram {
    pub module_id: String,
    pub path: PathBuf,
    pub identity: subc_os::FileIdentity,
    pub check: cortexkit_doctor::CheckOutput,
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
        for (id, path, result) in pool_until(
            jobs,
            cutoff,
            |(id, path)| {
                let result = check_one(
                    &path,
                    interrupt,
                    &options,
                    config.limits,
                    cutoff,
                    identity_lookup,
                );
                (id, path, result)
            },
            |(id, path)| (id, path, Job::NotStarted),
        ) {
            record(&mut pass, id, path, result);
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
fn check_one(
    path: &Path,
    interrupt: &Interrupt,
    options: &cortexkit_doctor::runner::Options,
    limits: cortexkit_doctor::runner::Limits,
    cutoff: Instant,
    identity_lookup: impl FnOnce(&Path) -> Option<subc_os::FileIdentity>,
) -> Job {
    if interrupt.signal().is_some() {
        return Job::NotStarted;
    }
    // Keep the pre-launch identity so repairs and issue reports can detect a
    // replaced program. The runner, not the identity lookup, diagnoses launch failures.
    let identity = identity_lookup(path);
    if Instant::now() > cutoff {
        return Job::NotStarted;
    }
    let Some(_guard) = interrupt.protect(
        "a module check",
        Instant::now() + limits.check + limits.term_to_kill.max(limits.exit_grace),
    ) else {
        return Job::NotStarted;
    };
    match (identity, cortexkit_doctor::runner::check(path, options)) {
        (Some(identity), Ok(output)) => Job::Checked(identity, output),
        (None, Ok(_)) => Job::IdentityUnavailable,
        (_, Err(error)) => Job::Failed(error),
    }
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
fn record(pass: &mut ProgramChecks, id: String, path: PathBuf, job: Job) {
    use cortexkit_doctor::{Crash, DecodeError, RunError};

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
                detail: None,
            });
            pass.checked.push(CheckedProgram {
                module_id: id,
                path,
                identity,
                check,
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
    pass.modules.push(row(id, outcome, detail));
}
