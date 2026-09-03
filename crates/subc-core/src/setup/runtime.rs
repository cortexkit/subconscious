use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{Map, Value};

use super::inventory::Inventory;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimePlatform {
    Macos,
    Linux,
    Windows,
}

impl RuntimePlatform {
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    pub const fn identifier(self) -> &'static str {
        match self {
            Self::Macos => "cortexkit.subc",
            Self::Linux => "cortexkit-subc.service",
            Self::Windows => "\\CortexKit\\subc-daemon",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePaths {
    pub definition: PathBuf,
    pub daemon: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeStatus {
    pub registered: bool,
    pub live: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandResult {
    pub success: bool,
    pub stdout: String,
    /// The service manager's own account of a refusal. Every registration
    /// and start step that fails must carry this into its error: a bare
    /// "could not register" sent the first macOS operator drive guessing at
    /// launchctl when launchctl had already said why.
    pub stderr: String,
}

impl CommandResult {
    /// stderr, then stdout, whichever the tool used to explain itself;
    /// `(no output)` when it said nothing, so the refusal never reads as if
    /// the reason were withheld by us.
    pub fn explanation(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        if text.is_empty() {
            "(no output)".to_string()
        } else {
            text.to_string()
        }
    }
}

pub trait CommandRunner {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String>;
}

pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|error| format!("could not run {program}: {error}"))?;
        Ok(CommandResult {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

pub fn runtime_paths(platform: RuntimePlatform, home: &Path, data_home: &Path) -> RuntimePaths {
    let daemon = home.join(platform_binary("ck-subc"));
    let definition = match platform {
        RuntimePlatform::Macos => data_home
            .join("Library")
            .join("LaunchAgents")
            .join("cortexkit.subc.plist"),
        RuntimePlatform::Linux => data_home
            .join(".config")
            .join("systemd")
            .join("user")
            .join("cortexkit-subc.service"),
        RuntimePlatform::Windows => data_home.join("cortexkit-subc-daemon.xml"),
    };
    RuntimePaths { definition, daemon }
}

pub fn desired_definition(platform: RuntimePlatform, paths: &RuntimePaths) -> String {
    let daemon = paths.daemon.to_string_lossy();
    match platform {
        RuntimePlatform::Macos => format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>cortexkit.subc</string><key>ProgramArguments</key><array><string>{daemon}</string></array><key>RunAtLoad</key><true/></dict></plist>\n"
        ),
        RuntimePlatform::Linux => format!(
            "[Unit]\nDescription=CortexKit subconscious daemon\n\n[Service]\nExecStart={daemon}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n"
        ),
        RuntimePlatform::Windows => format!(
            "<Task version=\"1.4\"><RegistrationInfo><URI>\\CortexKit\\subc-daemon</URI></RegistrationInfo><Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers><Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals><Actions Context=\"Author\"><Exec><Command>{daemon}</Command></Exec></Actions></Task>\n"
        ),
    }
}

/// Query persistence and current liveness independently. A registration that
/// starts only at a later login is not reported as a running daemon.
pub fn observe<R: CommandRunner>(
    platform: RuntimePlatform,
    runner: &mut R,
) -> Result<RuntimeStatus, String> {
    let registered = match platform {
        RuntimePlatform::Macos => {
            runner
                .run(
                    "launchctl",
                    &[
                        "print".to_string(),
                        format!("gui/{}/{}", current_uid(), platform.identifier()),
                    ],
                )?
                .success
        }
        RuntimePlatform::Linux => {
            runner
                .run(
                    "systemctl",
                    &[
                        "--user".to_string(),
                        "is-enabled".to_string(),
                        platform.identifier().to_string(),
                    ],
                )?
                .success
        }
        RuntimePlatform::Windows => {
            runner
                .run(
                    "schtasks.exe",
                    &[
                        "/Query".to_string(),
                        "/TN".to_string(),
                        platform.identifier().to_string(),
                    ],
                )?
                .success
        }
    };
    let live = match platform {
        RuntimePlatform::Macos => {
            let result = runner.run(
                "launchctl",
                &[
                    "print".to_string(),
                    format!("gui/{}/{}", current_uid(), platform.identifier()),
                ],
            )?;
            result.success && result.stdout.contains("state = running")
        }
        RuntimePlatform::Linux => {
            runner
                .run(
                    "systemctl",
                    &[
                        "--user".to_string(),
                        "is-active".to_string(),
                        platform.identifier().to_string(),
                    ],
                )?
                .success
        }
        RuntimePlatform::Windows => {
            let result = runner.run(
                "schtasks.exe",
                &[
                    "/Query".to_string(),
                    "/TN".to_string(),
                    platform.identifier().to_string(),
                    "/FO".to_string(),
                    "LIST".to_string(),
                ],
            )?;
            result.success
                && result
                    .stdout
                    .to_ascii_lowercase()
                    .contains("status: running")
        }
    };
    Ok(RuntimeStatus { registered, live })
}

pub fn ensure<R: CommandRunner>(
    platform: RuntimePlatform,
    paths: &RuntimePaths,
    status: RuntimeStatus,
    runner: &mut R,
    inventory: &mut Inventory,
) -> Result<(), String> {
    let desired = desired_definition(platform, paths);
    let needs_definition = fs::read_to_string(&paths.definition)
        .map(|current| current != desired)
        .unwrap_or(true);
    if needs_definition {
        let parent = paths.definition.parent().ok_or_else(|| {
            format!(
                "runtime definition {} has no parent",
                paths.definition.display()
            )
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create runtime directory {}: {error}",
                parent.display()
            )
        })?;
        fs::write(&paths.definition, desired).map_err(|error| {
            format!(
                "could not write runtime definition {}: {error}",
                paths.definition.display()
            )
        })?;
    }
    let mut fields = Map::new();
    fields.insert(
        "identifier".to_string(),
        Value::String(platform.identifier().to_string()),
    );
    inventory.record("runtime-definition", &paths.definition, fields);

    let mut live = status.live;
    if !status.registered {
        register(platform, paths, runner)?;
        let mut fields = Map::new();
        fields.insert(
            "identifier".to_string(),
            Value::String(platform.identifier().to_string()),
        );
        inventory.record("runtime-registration", &paths.definition, fields);
        // Registration starts the daemon on every platform (RunAtLoad,
        // `enable --now`, a logon task with immediate run). The liveness in
        // `status` was observed BEFORE that, so acting on it here would
        // start a daemon that is already up — and on macOS the old verb was
        // `kickstart -k`, which kills the fresh instance and respawns it, so
        // the validation that followed read the just-killed pid and called
        // the daemon dead. Re-observe after the mutating step.
        live = observe(platform, runner)?.live;
    }
    if !live {
        start(platform, paths, runner)?;
    }
    Ok(())
}

pub fn deregister<R: CommandRunner>(
    platform: RuntimePlatform,
    paths: &RuntimePaths,
    runner: &mut R,
) -> Result<(), String> {
    let args = match platform {
        RuntimePlatform::Macos => vec![
            "bootout".to_string(),
            format!("gui/{}", current_uid()),
            paths.definition.to_string_lossy().into_owned(),
        ],
        RuntimePlatform::Linux => vec![
            "--user".to_string(),
            "disable".to_string(),
            "--now".to_string(),
            platform.identifier().to_string(),
        ],
        RuntimePlatform::Windows => vec![
            "/Delete".to_string(),
            "/TN".to_string(),
            platform.identifier().to_string(),
            "/F".to_string(),
        ],
    };
    let program = match platform {
        RuntimePlatform::Macos => "launchctl",
        RuntimePlatform::Linux => "systemctl",
        RuntimePlatform::Windows => "schtasks.exe",
    };
    if runner.run(program, &args)?.success {
        Ok(())
    } else {
        Err(format!("could not deregister {}", platform.identifier()))
    }
}

fn register<R: CommandRunner>(
    platform: RuntimePlatform,
    paths: &RuntimePaths,
    runner: &mut R,
) -> Result<(), String> {
    let (program, args) = match platform {
        RuntimePlatform::Macos => (
            "launchctl",
            vec![
                "bootstrap".to_string(),
                format!("gui/{}", current_uid()),
                paths.definition.to_string_lossy().into_owned(),
            ],
        ),
        RuntimePlatform::Linux => (
            "systemctl",
            vec!["--user".to_string(), "daemon-reload".to_string()],
        ),
        RuntimePlatform::Windows => (
            "schtasks.exe",
            vec![
                "/Create".to_string(),
                "/TN".to_string(),
                platform.identifier().to_string(),
                "/XML".to_string(),
                paths.definition.to_string_lossy().into_owned(),
                "/F".to_string(),
            ],
        ),
    };
    let result = runner.run(program, &args)?;
    if !result.success {
        return Err(format!(
            "could not register {}: `{program} {}` said: {}",
            platform.identifier(),
            args.join(" "),
            result.explanation()
        ));
    }
    if platform == RuntimePlatform::Linux {
        let result = runner.run(
            "systemctl",
            &[
                "--user".to_string(),
                "enable".to_string(),
                "--now".to_string(),
                platform.identifier().to_string(),
            ],
        )?;
        if !result.success {
            return Err(format!(
                "could not enable cortexkit-subc.service for the user session: systemctl said: {}",
                result.explanation()
            ));
        }
    }
    Ok(())
}

fn start<R: CommandRunner>(
    platform: RuntimePlatform,
    _paths: &RuntimePaths,
    runner: &mut R,
) -> Result<(), String> {
    let (program, args) = match platform {
        // Without `-k`: starts the job if it is not running and is a no-op if
        // it is. `-k` means kill-and-restart, which is an upgrade's verb, not
        // setup's — setup must never take a running daemon down.
        RuntimePlatform::Macos => (
            "launchctl",
            vec![
                "kickstart".to_string(),
                format!("gui/{}/{}", current_uid(), platform.identifier()),
            ],
        ),
        // `enable --now` above is both the persistent registration and current
        // start. This arm only runs when a previously enabled unit is inactive.
        RuntimePlatform::Linux => (
            "systemctl",
            vec![
                "--user".to_string(),
                "start".to_string(),
                platform.identifier().to_string(),
            ],
        ),
        RuntimePlatform::Windows => (
            "schtasks.exe",
            vec![
                "/Run".to_string(),
                "/TN".to_string(),
                platform.identifier().to_string(),
            ],
        ),
    };
    let result = runner.run(program, &args)?;
    if result.success {
        Ok(())
    } else {
        Err(format!(
            "could not start {}: `{program} {}` said: {}",
            platform.identifier(),
            args.join(" "),
            result.explanation()
        ))
    }
}

fn platform_binary(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The uid whose launchd GUI domain owns the agent. Read from the kernel:
/// `$UID` is a shell variable that bash and zsh set for scripts and do not
/// export, so a real `ck` process never sees it, and the old fallback of 0
/// bootstrapped every macOS user's agent into root's domain
/// (`Bootstrap failed: 125: Domain does not support specified action`).
#[cfg(unix)]
fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use subc_core::test_support::TestTempDir;

    #[derive(Default)]
    struct RecordingRunner {
        calls: Vec<(String, Vec<String>)>,
        results: VecDeque<bool>,
    }

    impl CommandRunner for RecordingRunner {
        fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
            self.calls.push((program.to_string(), args.to_vec()));
            Ok(CommandResult {
                success: self.results.pop_front().unwrap_or(true),
                stdout: "state = running\nStatus: Running".to_string(),
                stderr: String::new(),
            })
        }
    }

    fn fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    /// Refuses every command with a reason on stderr, the way launchctl and
    /// systemctl actually do.
    struct RefusingRunner;

    impl CommandRunner for RefusingRunner {
        fn run(&mut self, _program: &str, _args: &[String]) -> Result<CommandResult, String> {
            Ok(CommandResult {
                success: false,
                stdout: String::new(),
                stderr: "Bootstrap failed: 5: Input/output error".to_string(),
            })
        }
    }

    /// Seventh finding of the macOS operator drive: registration failed with
    /// a bare "could not register cortexkit.subc" while launchctl had said
    /// exactly why on stderr and the runner threw it away. A service-manager
    /// refusal must carry the manager's own words and the command that drew
    /// them, so the operator (or the next drive) reads the cause instead of
    /// reproducing the step by hand to see it.
    #[test]
    fn registration_and_start_refusals_carry_the_service_managers_words() {
        let root = fixture_dir("refusal-words");
        let paths = runtime_paths(RuntimePlatform::Macos, root.path(), root.path());
        let error = register(RuntimePlatform::Macos, &paths, &mut RefusingRunner).unwrap_err();
        assert!(
            error.contains("Bootstrap failed: 5: Input/output error"),
            "{error}"
        );
        assert!(error.contains("launchctl bootstrap"), "{error}");
        let error = start(RuntimePlatform::Macos, &paths, &mut RefusingRunner).unwrap_err();
        assert!(error.contains("Bootstrap failed"), "{error}");
        assert!(error.contains("launchctl kickstart"), "{error}");
    }

    /// Eighth finding of the macOS operator drive: the agent was bootstrapped
    /// into `gui/0` on every Mac because the uid came from `$UID`, a shell
    /// variable no shell exports. The domain must be the real user's, and it
    /// must not depend on anything in the environment.
    #[cfg(unix)]
    #[test]
    fn macos_agent_domain_is_the_real_users_not_roots_and_not_from_the_environment() {
        let root = fixture_dir("gui-domain");
        let paths = runtime_paths(RuntimePlatform::Macos, root.path(), root.path());
        let mut runner = RecordingRunner::default();
        // Poison the environment the old code trusted; the kernel must win.
        let previous = std::env::var_os("UID");
        std::env::set_var("UID", "0");
        let result = register(RuntimePlatform::Macos, &paths, &mut runner);
        match previous {
            Some(value) => std::env::set_var("UID", value),
            None => std::env::remove_var("UID"),
        }
        result.expect("register");
        let domain = &runner.calls[0].1[1];
        let real = rustix::process::getuid().as_raw();
        assert_eq!(domain, &format!("gui/{real}"));
        assert_ne!(real, 0, "this test is meaningless as root");
    }

    /// Ninth finding of the macOS operator drive: `ensure` registered (which
    /// starts the daemon) and then started again on the PRE-registration
    /// liveness snapshot — on macOS with `kickstart -k`, a kill-and-restart
    /// of the instance it had just brought up. Liveness must be re-observed
    /// after the mutating step, and setup must never use a restart verb.
    #[test]
    fn ensure_reobserves_liveness_after_registering_and_never_restarts() {
        let root = fixture_dir("ensure-order");
        let paths = runtime_paths(RuntimePlatform::Macos, root.path(), root.path());
        let mut inventory = Inventory::load(root.join("installer-manifest.json"), "darwin-arm64")
            .expect("inventory");
        // Every command succeeds and reports `state = running`: bootstrap
        // brought the daemon up, so the re-observation sees it live.
        let mut runner = RecordingRunner::default();
        ensure(
            RuntimePlatform::Macos,
            &paths,
            RuntimeStatus {
                registered: false,
                live: false,
            },
            &mut runner,
            &mut inventory,
        )
        .expect("ensure");
        let verbs: Vec<&str> = runner
            .calls
            .iter()
            .map(|(_, args)| args[0].as_str())
            .collect();
        assert_eq!(
            verbs,
            ["bootstrap", "print", "print"],
            "register, then re-observe (registered + live); no start on a live daemon"
        );
        // (The -k check that stood here was vacuous: on this path `start`
        // never runs. The verb is pinned where it fires, below.)
    }

    /// The start verb itself, on the one path that reaches it: registration
    /// that did not bring the job up (bootstrapped but not running — the
    /// job disabled, or the process gone at spawn). The verb must be a
    /// start, never a kill-and-restart.
    #[test]
    fn macos_start_verb_is_kickstart_without_kill() {
        let root = fixture_dir("start-verb");
        let paths = runtime_paths(RuntimePlatform::Macos, root.path(), root.path());
        let mut inventory = Inventory::load(root.join("installer-manifest.json"), "darwin-arm64")
            .expect("inventory");
        // Registered after bootstrap, but never live: `state = waiting`.
        let mut runner = InactiveRunner {
            calls: Vec::new(),
            inactive_output: "state = waiting".to_string(),
        };
        ensure(
            RuntimePlatform::Macos,
            &paths,
            RuntimeStatus {
                registered: false,
                live: false,
            },
            &mut runner,
            &mut inventory,
        )
        .expect("ensure");
        let start = runner
            .calls
            .iter()
            .find(|(_, args)| args.first().map(String::as_str) == Some("kickstart"))
            .expect("a not-live registration must be started");
        assert_eq!(
            start.1,
            [
                "kickstart",
                &format!("gui/{}/cortexkit.subc", current_uid())
            ],
            "no -k: setup starts, it never kill-and-restarts"
        );
    }

    #[test]
    fn explanation_prefers_stderr_and_never_reads_as_withheld() {
        let both = CommandResult {
            success: false,
            stdout: "out".into(),
            stderr: "err".into(),
        };
        assert_eq!(both.explanation(), "err");
        let only_out = CommandResult {
            success: false,
            stdout: "out".into(),
            stderr: "  ".into(),
        };
        assert_eq!(only_out.explanation(), "out");
        assert_eq!(CommandResult::default().explanation(), "(no output)");
    }

    #[test]
    fn registration_and_current_liveness_are_separate_observations() {
        let mut runner = RecordingRunner {
            results: VecDeque::from([true, false]),
            ..Default::default()
        };
        let status = observe(RuntimePlatform::Linux, &mut runner).expect("observe runtime");
        assert!(status.registered);
        assert!(!status.live);
        assert_eq!(runner.calls[0].1[1], "is-enabled");
        assert_eq!(runner.calls[1].1[1], "is-active");
    }

    #[test]
    fn persistent_registration_is_not_mistaken_for_liveness_on_macos_or_windows() {
        for (platform, inactive_output) in [
            (RuntimePlatform::Macos, "state = waiting"),
            (RuntimePlatform::Windows, "Status: Ready"),
        ] {
            // A successful service-manager query can still report an inactive
            // job. Persistence alone must not pass setup.
            let mut inactive_runner = InactiveRunner {
                calls: Vec::new(),
                inactive_output: inactive_output.to_string(),
            };
            let inactive =
                observe(platform, &mut inactive_runner).expect("observe inactive runtime");
            assert!(inactive.registered);
            assert!(
                !inactive.live,
                "{platform:?} registration must not imply liveness"
            );
        }
    }

    struct InactiveRunner {
        calls: Vec<(String, Vec<String>)>,
        inactive_output: String,
    }

    impl CommandRunner for InactiveRunner {
        fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
            self.calls.push((program.to_string(), args.to_vec()));
            Ok(CommandResult {
                success: true,
                stdout: if self.calls.len() == 1 {
                    String::new()
                } else {
                    self.inactive_output.clone()
                },
                stderr: String::new(),
            })
        }
    }

    /// Reports the job as live only once a registering verb has run and,
    /// for platforms whose registration does not start the job (`schtasks
    /// /Create`), only once the start verb has run too. Models the service
    /// managers rather than answering "running" unconditionally, so the
    /// start arm is proven to fire exactly when registration alone did not
    /// bring the daemon up.
    #[derive(Default)]
    struct ServiceManagerModel {
        calls: Vec<(String, Vec<String>)>,
        registered: bool,
        started: bool,
    }

    impl CommandRunner for ServiceManagerModel {
        fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
            self.calls.push((program.to_string(), args.to_vec()));
            let joined = args.join(" ");
            if joined.starts_with("bootstrap ") || joined.starts_with("--user enable --now") {
                // RunAtLoad / enable --now: registering starts the job.
                self.registered = true;
                self.started = true;
            } else if joined.starts_with("/Create ") {
                // A task definition does not run.
                self.registered = true;
            } else if joined.starts_with("kickstart ")
                || joined.starts_with("/Run ")
                || joined.starts_with("--user start ")
            {
                self.started = true;
            }
            let live = self.registered && self.started;
            // Mutating verbs succeed; only queries (`print`, `is-enabled`,
            // `is-active`, `/Query`) answer from state.
            let is_query = joined.starts_with("print ")
                || joined.starts_with("--user is-")
                || joined.starts_with("/Query ");
            let success = if is_query {
                if joined.starts_with("--user is-active") {
                    live
                } else {
                    self.registered
                }
            } else {
                true
            };
            Ok(CommandResult {
                success,
                stdout: if live {
                    "state = running\nStatus: Running".to_string()
                } else {
                    "state = waiting\nStatus: Ready".to_string()
                },
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn setup_registers_and_starts_the_platform_runtime_now() {
        for platform in [
            RuntimePlatform::Macos,
            RuntimePlatform::Linux,
            RuntimePlatform::Windows,
        ] {
            let root = fixture_dir(platform.identifier());
            let paths = runtime_paths(platform, &root.join("bin"), &root);
            let manifest = root.join("installer-manifest.json");
            let mut inventory = Inventory::load(&manifest, "test").expect("inventory");
            let mut runner = ServiceManagerModel::default();
            ensure(
                platform,
                &paths,
                RuntimeStatus::default(),
                &mut runner,
                &mut inventory,
            )
            .expect("register and start");
            let rendered = runner
                .calls
                .iter()
                .map(|(_, args)| args.join(" "))
                .collect::<Vec<_>>()
                .join(" | ");
            let after = observe(platform, &mut runner).expect("observe after ensure");
            assert!(after.registered && after.live, "{platform:?}: {rendered}");
            match platform {
                // bootstrap starts the job; no kickstart may follow.
                RuntimePlatform::Macos => {
                    assert!(rendered.contains("bootstrap"), "{rendered}");
                    assert!(!rendered.contains("kickstart"), "{rendered}");
                }
                // enable --now starts the job; no separate start may follow.
                RuntimePlatform::Linux => {
                    assert!(rendered.contains("--user enable --now"), "{rendered}");
                    assert!(!rendered.contains("--user start"), "{rendered}");
                }
                // /Create only defines the task; /Run must follow.
                RuntimePlatform::Windows => {
                    assert!(rendered.contains("/Create"), "{rendered}");
                    assert!(rendered.contains("/Run"), "{rendered}");
                }
            }
            assert!(inventory.owns_path("runtime-definition", &paths.definition));
        }
    }
}
