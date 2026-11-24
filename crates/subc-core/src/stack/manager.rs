use super::{Budget, ManagerState};
use crate::setup::runtime::{self, CommandResult, CommandRunner, RuntimePaths, RuntimePlatform};
use crate::setup::systemd_unit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Job {
    pub state: ManagerState,
    pub pid: Option<u32>,
}

impl Job {
    fn new(state: ManagerState) -> Self {
        Self { state, pid: None }
    }

    pub fn live(self) -> bool {
        self.state == ManagerState::Running
    }
}

fn failed(program: &str, args: &[String], result: &CommandResult) -> String {
    format!(
        "`{program} {}` said: {}",
        args.join(" "),
        result.explanation()
    )
}

fn launchd_target(platform: RuntimePlatform) -> String {
    format!("gui/{}/{}", runtime::current_uid(), platform.identifier())
}

// Use the same observation commands as setup, but keep negative states
// separate from command failures and unrecognized output.
pub(crate) fn observe(
    platform: RuntimePlatform,
    runner: &mut impl CommandRunner,
) -> Result<Job, String> {
    use ManagerState::{Loaded, NotLoaded, Running};
    let unit = platform.identifier();
    match platform {
        RuntimePlatform::Macos => {
            let args = ["print".into(), launchd_target(platform)];
            let result = runner.run("launchctl", &args)?;
            if !result.success {
                let explanation = result.explanation();
                if explanation.contains(&format!("Could not find service \"{unit}\" in domain")) {
                    return Ok(Job::new(NotLoaded));
                }
                return Err(failed("launchctl", &args, &result));
            }
            let property = |name| {
                result.stdout.lines().find_map(|line| {
                    let (key, value) = line.trim().split_once(" = ")?;
                    (key == name).then(|| value.trim())
                })
            };
            let pid = property("pid")
                .and_then(|value| value.parse().ok())
                .filter(|pid| *pid > 0);
            match (property("state"), pid) {
                (Some("running"), _) | (_, Some(_)) => Ok(Job {
                    state: Running,
                    pid,
                }),
                (Some("not running" | "waiting" | "exited"), None) => Ok(Job::new(Loaded)),
                _ => Err(format!(
                    "launchctl did not report a conclusive state: {}",
                    result.explanation()
                )),
            }
        }
        RuntimePlatform::Linux => {
            let enabled_args = ["--user".into(), "is-enabled".into(), unit.into()];
            let enabled = runner.run("systemctl", &enabled_args)?;
            let absent = enabled.stdout.trim() == "not-found"
                || (!enabled.success
                    && enabled.stderr.contains(unit)
                    && enabled.stderr.contains("No such file or directory"));
            let registered = if absent {
                false
            } else if enabled.success
                || matches!(
                    enabled.stdout.trim(),
                    "disabled" | "masked" | "masked-runtime" | "bad"
                )
            {
                true
            } else {
                return Err(failed("systemctl", &enabled_args, &enabled));
            };
            let active_args = ["--user".into(), "is-active".into(), unit.into()];
            let active = runner.run("systemctl", &active_args)?;
            match (active.success, active.stdout.trim()) {
                (true, "active" | "reloading" | "refreshing") => Ok(Job::new(Running)),
                // A transition can still have a running daemon. Do not treat it
                // as stopped, even when is-active returns a nonzero status.
                (_, "activating" | "deactivating") => Ok(Job::new(Running)),
                (false, "inactive" | "failed") => {
                    Ok(Job::new(if registered { Loaded } else { NotLoaded }))
                }
                (false, "unknown") if !registered => Ok(Job::new(NotLoaded)),
                _ => Err(failed("systemctl", &active_args, &active)),
            }
        }
        RuntimePlatform::Windows => {
            let args = [
                "/Query".into(),
                "/TN".into(),
                unit.into(),
                "/FO".into(),
                "LIST".into(),
            ];
            let result = runner.run("schtasks.exe", &args)?;
            if !result.success {
                return Err(failed("schtasks.exe", &args, &result));
            }
            let status = result.stdout.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case("status")
                    .then(|| value.trim().to_ascii_lowercase())
            });
            match status.as_deref() {
                Some("running") => Ok(Job::new(Running)),
                Some("ready" | "disabled" | "queued" | "could not start") => Ok(Job::new(Loaded)),
                _ => Err(format!(
                    "schtasks.exe did not report a conclusive status: {}",
                    result.explanation()
                )),
            }
        }
    }
}

pub(super) fn stop(
    platform: RuntimePlatform,
    paths: &RuntimePaths,
    runner: &mut BoundedRunner<'_>,
) -> Result<(), String> {
    let unit = platform.identifier();
    let (program, args) = match platform {
        RuntimePlatform::Linux => {
            systemd_unit::refuse_foreign_fragment(&paths.definition, &[], runner)?;
            (
                "systemctl",
                vec!["--user".into(), "stop".into(), unit.into()],
            )
        }
        RuntimePlatform::Macos => (
            "launchctl",
            vec!["bootout".into(), launchd_target(platform)],
        ),
        RuntimePlatform::Windows => (
            "schtasks.exe",
            vec!["/End".into(), "/TN".into(), unit.into()],
        ),
    };
    runner.mutation_started = false;
    let stopped = runner.run(program, &args).and_then(|result| {
        if result.success {
            Ok(())
        } else {
            Err(failed(program, &args, &result))
        }
    });
    stopped.or_else(|error| {
        if runner.mutation_started
            && observe(platform, runner).is_ok_and(|job| {
                job.state
                    == if platform == RuntimePlatform::Windows {
                        ManagerState::Loaded
                    } else {
                        ManagerState::NotLoaded
                    }
            })
        {
            Ok(())
        } else {
            Err(error)
        }
    })
}

pub(super) fn start(
    platform: RuntimePlatform,
    paths: &RuntimePaths,
    job: Job,
    runner: &mut BoundedRunner<'_>,
) -> Result<(), String> {
    if job.state == ManagerState::NotLoaded {
        return if platform == RuntimePlatform::Macos {
            runtime::register(platform, paths, runner)
        } else {
            Err("the daemon service isn't loaded; run ck setup".into())
        };
    }
    if platform == RuntimePlatform::Linux {
        systemd_unit::verify_fragment(&paths.definition, runner)?;
    }
    runtime::start(platform, paths, runner)
}

pub(super) struct BoundedRunner<'a> {
    inner: &'a mut dyn CommandRunner,
    budget: Budget<'a>,
    pub overrun: Option<String>,
    pub mutation_started: bool,
}

impl<'a> BoundedRunner<'a> {
    pub fn new(inner: &'a mut dyn CommandRunner, budget: Budget<'a>) -> Self {
        Self {
            inner,
            budget,
            overrun: None,
            mutation_started: false,
        }
    }
}

impl CommandRunner for BoundedRunner<'_> {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        let mutating = args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "bootout" | "bootstrap" | "kickstart" | "start" | "stop" | "/End" | "/Run"
            )
        });
        let before = self.budget.remaining();
        let name = || format!("{program} {}", args.join(" "));
        if mutating {
            if before.is_zero() {
                return Err(format!("deadline passed before `{}` could run", name()));
            }
            self.mutation_started = true;
        }
        // A synchronous manager command may finish after the deadline. Keep the
        // locks until it returns; neither cancel it nor allow a later mutation.
        let result = self.inner.run(program, args);
        if !before.is_zero() && self.budget.overdue() && self.overrun.is_none() {
            self.overrun = Some(format!(
                "service manager call `{}` took longer than the deadline",
                name()
            ));
        }
        result
    }
}
