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

    if !status.registered {
        register(platform, paths, runner)?;
        let mut fields = Map::new();
        fields.insert(
            "identifier".to_string(),
            Value::String(platform.identifier().to_string()),
        );
        inventory.record("runtime-registration", &paths.definition, fields);
    }
    if !status.live {
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
    if !runner.run(program, &args)?.success {
        return Err(format!("could not register {}", platform.identifier()));
    }
    if platform == RuntimePlatform::Linux
        && !runner
            .run(
                "systemctl",
                &[
                    "--user".to_string(),
                    "enable".to_string(),
                    "--now".to_string(),
                    platform.identifier().to_string(),
                ],
            )?
            .success
    {
        return Err("could not enable cortexkit-subc.service for the user session".to_string());
    }
    Ok(())
}

fn start<R: CommandRunner>(
    platform: RuntimePlatform,
    _paths: &RuntimePaths,
    runner: &mut R,
) -> Result<(), String> {
    let (program, args) = match platform {
        RuntimePlatform::Macos => (
            "launchctl",
            vec![
                "kickstart".to_string(),
                "-k".to_string(),
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
    if runner.run(program, &args)?.success {
        Ok(())
    } else {
        Err(format!("could not start {}", platform.identifier()))
    }
}

fn platform_binary(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn current_uid() -> u32 {
    std::env::var("UID")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
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
            })
        }
    }

    fn fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
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
            let mut runner = RecordingRunner::default();
            ensure(
                platform,
                &paths,
                RuntimeStatus::default(),
                &mut runner,
                &mut inventory,
            )
            .expect("register and start");
            let rendered = format!("{:?}", runner.calls);
            match platform {
                RuntimePlatform::Macos => {
                    assert!(rendered.contains("bootstrap") && rendered.contains("kickstart"))
                }
                RuntimePlatform::Linux => {
                    assert!(rendered.contains("enable") && rendered.contains("--now"))
                }
                RuntimePlatform::Windows => {
                    assert!(rendered.contains("/Create") && rendered.contains("/Run"))
                }
            }
            assert!(inventory.owns_path("runtime-definition", &paths.definition));
        }
    }
}
