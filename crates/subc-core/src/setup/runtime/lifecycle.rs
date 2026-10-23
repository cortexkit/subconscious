use super::*;
use subc_transport::connection_file;

fn image_matches(image: &Path, daemon: &Path) -> bool {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .trim_end_matches(" (deleted)")
            .replace('\\', "/")
    };
    let mut image = normalize(image);
    let mut daemon = normalize(daemon);
    if cfg!(windows) {
        image.make_ascii_lowercase();
        daemon.make_ascii_lowercase();
    }
    if image == daemon {
        return true;
    }
    // Windows keeps a running image beside its replacement until it exits.
    let Some((parent, name)) = daemon.rsplit_once('/') else {
        return false;
    };
    image.starts_with(&format!("{parent}/.{name}.replaced-"))
        && !image[parent.len() + 1..].contains('/')
}

pub(super) fn daemon_processes<R: CommandRunner + ?Sized>(
    runner: &mut R,
    daemon: &Path,
) -> Result<Vec<DaemonProcess>, String> {
    let result = if cfg!(windows) {
        runner.run("powershell.exe", &[
            "-NoProfile".into(), "-NonInteractive".into(), "-Command".into(),
            "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); ConvertTo-Json -Compress -InputObject @(Get-CimInstance Win32_Process | Select-Object ProcessId,ExecutablePath)".into(),
        ])?
    } else {
        runner.run("ps", &["-ww".into(), "-axo".into(), "pid=,comm=".into()])?
    };
    if !result.success {
        return Err(format!(
            "could not inspect daemon processes: {}",
            result.explanation()
        ));
    }
    let images: Vec<(u32, PathBuf)> = if cfg!(windows) {
        let rows: Vec<Value> = serde_json::from_str(result.stdout.trim())
            .map_err(|error| format!("invalid process list: {error}"))?;
        rows.iter()
            .filter_map(|row| {
                Some((
                    u32::try_from(row.get("ProcessId")?.as_u64()?).ok()?,
                    PathBuf::from(row.get("ExecutablePath")?.as_str()?),
                ))
            })
            .collect()
    } else {
        result
            .stdout
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let (pid, image) = line.split_once(char::is_whitespace)?;
                let pid: u32 = pid.parse().ok()?;
                // Linux's comm is only a short task name, not its executable path.
                let image = if cfg!(target_os = "linux") {
                    fs::read_link(format!("/proc/{pid}/exe")).ok()?
                } else {
                    PathBuf::from(image.trim())
                };
                Some((pid, image))
            })
            .collect()
    };
    Ok(images
        .into_iter()
        .filter(|(_, image)| image_matches(image, daemon))
        .filter_map(|(pid, _)| {
            Some(DaemonProcess {
                pid,
                start_time: subc_os::start_time(pid)?,
            })
        })
        .collect())
}

pub(super) fn connected_daemon<R: CommandRunner>(
    runner: &mut R,
    daemon: &Path,
    connection: Option<&Path>,
) -> Result<Option<DaemonProcess>, String> {
    let Ok(discovered) = connection_file::discover(connection) else {
        return Ok(None);
    };
    Ok(runner
        .daemon_processes(daemon)?
        .into_iter()
        .find(|process| process.pid == discovered.info.pid))
}

pub(super) fn wait_for_exit<R: CommandRunner>(
    runner: &mut R,
    old: Option<&DaemonProcess>,
    timeout: Duration,
) -> Result<(), String> {
    // Stop requests may return before the process exits. Never launch a peer
    // while the old incarnation can still win the singleton probe.
    let Some(old) = old else {
        return Ok(());
    };
    let deadline = Instant::now() + timeout;
    while runner.process_running(old) {
        if Instant::now() >= deadline {
            return Err(format!("refusal: daemon-stop-timeout: daemon pid {} did not exit within {} seconds; replacement was not started", old.pid, timeout.as_secs()));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[derive(Debug)]
struct ServiceObservation {
    finished: bool,
    state: String,
    last_result: String,
    process_present: bool,
}

fn service_observation<R: CommandRunner>(
    platform: RuntimePlatform,
    paths: &RuntimePaths,
    runner: &mut R,
) -> Result<ServiceObservation, String> {
    let (program, args) = match platform {
        RuntimePlatform::Windows => ("powershell.exe", vec![
            "-NoProfile".into(), "-NonInteractive".into(), "-Command".into(),
            "$ErrorActionPreference='Stop'; $t=Get-ScheduledTask -TaskPath '\\CortexKit\\' -TaskName 'subc-daemon'; $i=$t | Get-ScheduledTaskInfo; @{state=$t.State.ToString(); result=('0x{0:X8}' -f $i.LastTaskResult)} | ConvertTo-Json -Compress".into(),
        ]),
        RuntimePlatform::Macos => ("launchctl", vec!["print".into(),
            format!("gui/{}/{}", current_uid(), platform.identifier())]),
        RuntimePlatform::Linux => ("systemctl", vec!["--user".into(), "show".into(),
            platform.identifier().into(), "--property=ActiveState,SubState,ExecMainStatus,Result".into()]),
    };
    let result = runner.run(program, &args)?;
    if !result.success {
        return Err(format!(
            "could not observe daemon job: {}",
            result.explanation()
        ));
    }
    let (state, last_result, finished) = match platform {
        RuntimePlatform::Windows => {
            let value: Value = serde_json::from_str(&result.stdout)
                .map_err(|error| format!("invalid task state: {error}"))?;
            let state = value["state"].as_str().unwrap_or("unknown").to_string();
            let last_result = value["result"].as_str().unwrap_or("unknown").to_string();
            let finished = matches!(state.as_str(), "Ready" | "Disabled");
            (state, last_result, finished)
        }
        RuntimePlatform::Macos => {
            let field = |name: &str| {
                result
                    .stdout
                    .lines()
                    .find_map(|line| line.trim().strip_prefix(name))
                    .unwrap_or("unknown")
                    .to_string()
            };
            let state = field("state = ");
            let finished = matches!(state.as_str(), "not running" | "exited" | "waiting");
            (state, field("last exit code = "), finished)
        }
        RuntimePlatform::Linux => {
            let field = |name: &str| {
                result
                    .stdout
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .unwrap_or("unknown")
                    .to_string()
            };
            let active = field("ActiveState=");
            let finished = matches!(active.as_str(), "inactive" | "failed");
            (
                format!("{active}/{}", field("SubState=")),
                format!("{} (status {})", field("Result="), field("ExecMainStatus=")),
                finished,
            )
        }
    };
    let process_present = !runner.daemon_processes(&paths.daemon)?.is_empty();
    Ok(ServiceObservation {
        finished,
        state,
        last_result,
        process_present,
    })
}

pub(crate) fn wait_for_service_ready<R: CommandRunner>(
    platform: RuntimePlatform,
    paths: &RuntimePaths,
    runner: &mut R,
    timeout: Duration,
    mut ready: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut retried = false;
    loop {
        if ready()? {
            return Ok(());
        }
        let observation = service_observation(platform, paths, runner)?;
        let detail = format!(
            "process {}; task/job state {}; last result {}",
            if observation.process_present {
                "present"
            } else {
                "absent"
            },
            observation.state,
            observation.last_result
        );
        if observation.finished && !observation.process_present {
            if retried {
                return Err(format!(
                    "the daemon did not stay running after one start retry: {detail}"
                ));
            }
            start(platform, paths, runner).map_err(|error| {
                format!("the daemon did not stay running: {detail}; start retry failed: {error}")
            })?;
            retried = true;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "completion did not become healthy within {} seconds: {detail}",
                timeout.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Manager {
        polls: usize,
        starts: usize,
        present: bool,
        linger: bool,
        never_exits: bool,
        observation: Option<String>,
    }

    impl CommandRunner for Manager {
        fn run(&mut self, _program: &str, args: &[String]) -> Result<CommandResult, String> {
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "/Run" | "bootstrap" | "kickstart" | "start"))
            {
                assert!(
                    !self.linger || self.polls >= 3,
                    "start raced the old daemon"
                );
                self.starts += 1;
            }
            Ok(CommandResult {
                success: true,
                stdout: self
                    .observation
                    .clone()
                    .unwrap_or_else(|| r#"{"state":"Ready","result":"0x00000000"}"#.into()),
                stderr: String::new(),
            })
        }

        fn daemon_processes(&mut self, _: &Path) -> Result<Vec<DaemonProcess>, String> {
            Ok(if self.present {
                vec![DaemonProcess {
                    pid: 123,
                    start_time: 1,
                }]
            } else {
                vec![]
            })
        }

        fn process_running(&mut self, _: &DaemonProcess) -> bool {
            self.polls += 1;
            self.never_exits || self.polls < 3
        }
    }

    fn paths() -> RuntimePaths {
        RuntimePaths {
            daemon: "ck-subc.exe".into(),
            definition: "daemon.xml".into(),
        }
    }

    #[test]
    fn restart_waits_for_lingering_daemon_before_start_on_windows_and_macos() {
        for platform in [RuntimePlatform::Windows, RuntimePlatform::Macos] {
            let mut manager = Manager {
                linger: true,
                ..Manager::default()
            };
            restart_via_service_manager_on(
                platform,
                &paths().definition,
                &mut manager,
                Some(&DaemonProcess {
                    pid: 123,
                    start_time: 1,
                }),
            )
            .unwrap();
            assert_eq!(manager.starts, 1);
            assert_eq!(manager.polls, 3);
        }
    }

    #[test]
    fn restart_refuses_with_pid_when_old_daemon_does_not_exit() {
        let mut manager = Manager {
            never_exits: true,
            ..Manager::default()
        };
        let error = wait_for_exit(
            &mut manager,
            Some(&DaemonProcess {
                pid: 123,
                start_time: 1,
            }),
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(
            error.contains("daemon-stop-timeout") && error.contains("123"),
            "{error}"
        );
        assert_eq!(manager.starts, 0);
    }

    #[test]
    fn completion_retries_once_then_reports_finished_task_without_process() {
        let mut manager = Manager::default();
        let error = wait_for_service_ready(
            RuntimePlatform::Windows,
            &paths(),
            &mut manager,
            Duration::from_secs(5),
            || Ok(false),
        )
        .unwrap_err();
        assert_eq!(manager.starts, 1);
        assert!(error.contains("the daemon did not stay running"), "{error}");
        assert!(
            error.contains("process absent")
                && error.contains("Ready")
                && error.contains("0x00000000"),
            "{error}"
        );
    }

    #[test]
    fn completion_accepts_a_healthy_retry() {
        let mut manager = Manager::default();
        let mut probes = 0;
        wait_for_service_ready(
            RuntimePlatform::Windows,
            &paths(),
            &mut manager,
            Duration::from_secs(5),
            || {
                probes += 1;
                Ok(probes >= 2)
            },
        )
        .unwrap();
        assert_eq!(manager.starts, 1);
    }

    #[test]
    fn completion_does_not_retry_a_process_that_is_still_starting() {
        let mut manager = Manager {
            present: true,
            ..Manager::default()
        };
        let error = wait_for_service_ready(
            RuntimePlatform::Windows,
            &paths(),
            &mut manager,
            Duration::ZERO,
            || Ok(false),
        )
        .unwrap_err();
        assert_eq!(manager.starts, 0);
        assert!(error.contains("process present"), "{error}");
    }

    #[test]
    fn completion_diagnoses_finished_jobs_on_macos_and_linux() {
        for (platform, observation) in [
            (
                RuntimePlatform::Macos,
                "state = not running\nlast exit code = 0",
            ),
            (
                RuntimePlatform::Linux,
                "ActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainStatus=1",
            ),
        ] {
            let mut manager = Manager {
                observation: Some(observation.into()),
                ..Manager::default()
            };
            let error = wait_for_service_ready(
                platform,
                &paths(),
                &mut manager,
                Duration::from_secs(5),
                || Ok(false),
            )
            .unwrap_err();
            assert_eq!(manager.starts, 1);
            assert!(
                error.contains("the daemon did not stay running")
                    && error.contains("process absent"),
                "{error}"
            );
        }
    }

    #[test]
    fn completion_does_not_retry_a_queued_task_without_a_process_yet() {
        let mut manager = Manager {
            observation: Some(r#"{"state":"Queued","result":"0x00041325"}"#.into()),
            ..Manager::default()
        };
        let error = wait_for_service_ready(
            RuntimePlatform::Windows,
            &paths(),
            &mut manager,
            Duration::ZERO,
            || Ok(false),
        )
        .unwrap_err();
        assert_eq!(manager.starts, 0);
        assert!(
            error.contains("Queued") && error.contains("process absent"),
            "{error}"
        );
    }

    #[test]
    fn process_scan_finds_the_actual_executable_not_just_its_short_name() {
        let image = std::env::current_exe().unwrap();
        let processes = daemon_processes(&mut SystemCommandRunner, &image).unwrap();
        assert!(processes
            .iter()
            .any(|process| process.pid == std::process::id()));
    }

    #[test]
    fn image_check_accepts_replaced_image_but_not_an_unrelated_pid() {
        let daemon = Path::new("/bin/ck-subc.exe");
        assert!(image_matches(
            Path::new("/bin/.ck-subc.exe.replaced-42-1"),
            daemon
        ));
        assert!(!image_matches(Path::new("/elsewhere/ck-subc.exe"), daemon));
        assert!(!image_matches(Path::new("/bin/not-ck-subc.exe"), daemon));
    }
}
