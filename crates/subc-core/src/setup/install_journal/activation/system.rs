use super::{daemon_build, module_build, Build, Control, Intent, Live, Prior};
use crate::setup::{runtime, systemd_unit};
use serde_json::Value;
use std::path::Path;

pub(in crate::setup) struct SystemControl<'a, R> {
    pub executable: &'a Path,
    pub connection: Option<&'a Path>,
    pub platform: runtime::RuntimePlatform,
    pub paths: &'a runtime::RuntimePaths,
    pub module_image: &'a Path,
    pub expected_declaration: Option<&'a Build>,
    pub runner: &'a mut R,
}

struct Observation {
    identity: String,
    facts: Value,
}

impl<R: runtime::CommandRunner> SystemControl<'_, R> {
    pub fn reported_module_build(&mut self, module: &str) -> Option<Build> {
        let observation = self.observe(Some(module)).ok().flatten()?;
        daemon_build(&observation.facts["module_declared"]["build"]).ok()
    }
    fn ck(&mut self, args: &[&str]) -> Result<String, String> {
        let mut argv = Vec::new();
        if let Some(connection) = self.connection {
            argv.push("--subc".into());
            argv.push(connection.to_string_lossy().into_owned());
        }
        argv.extend(args.iter().copied().map(str::to_owned));
        let result = self.runner.run(&self.executable.to_string_lossy(), &argv)?;
        if !result.success {
            return Err(format!(
                "ck {} failed: {}",
                args.join(" "),
                result.explanation()
            ));
        }
        Ok(result.stdout)
    }

    fn module_enabled(&mut self, module: &str) -> Result<Option<bool>, String> {
        let response = self.ck(&["--json", "module", "list"])?;
        let value: Value = serde_json::from_str(&response)
            .map_err(|error| format!("invalid authenticated module list: {error}"))?;
        let rows = value["modules"]
            .as_array()
            .ok_or("authenticated module list omitted its modules")?;
        rows.iter()
            .find(|row| row["module_id"] == module)
            .map(|row| {
                row["enabled"].as_bool().ok_or_else(|| {
                    format!("authenticated module list omitted whether {module} is enabled")
                })
            })
            .transpose()
    }

    fn observe(&mut self, module: Option<&str>) -> Result<Option<Observation>, String> {
        if let Some(module) = module {
            let response = self.ck(&["--json", "provenance", module])?;
            let value: Value = serde_json::from_str(&response)
                .map_err(|error| format!("invalid provenance for {module}: {error}"))?;
            let module_records = value["modules"]
                .as_array()
                .ok_or("authenticated provenance has no module list")?;
            let Some(record) = module_records.iter().find(|row| row["module_id"] == module) else {
                return Ok(None);
            };
            let observed = &record["daemon_observed"];
            let Some(pid) = observed["pid"].as_u64() else {
                if observed["running_image"]["reason"] == "not_running" {
                    return Ok(None);
                }
                return Err(format!(
                    "authenticated provenance did not establish whether {module} is running"
                ));
            };
            let started = observed["spawned_at_ms"].as_u64().ok_or_else(|| {
                format!("authenticated provenance omitted the spawn time for {module}")
            })?;
            return Ok(Some(Observation {
                identity: format!("{pid}:{started}"),
                facts: record.clone(),
            }));
        }

        if !runtime::observe(self.platform, self.runner)?.live {
            return Ok(None);
        }
        self.observe_running_daemon().map(Some)
    }

    fn observe_running_daemon(&mut self) -> Result<Observation, String> {
        // Pin the chosen path and check it again after the authenticated
        // request. A publication racing the dial must not lend its daemon id
        // to the answer from an older process.
        let selected = subc_transport::connection_file::discover_reachable(self.connection)
            .map_err(|error| error.to_string())?
            .discovered;
        let executable = self.executable.to_string_lossy();
        let result = self.runner.run(
            &executable,
            &[
                "--subc".into(),
                selected.path.to_string_lossy().into_owned(),
                "--json".into(),
                "daemon".into(),
            ],
        )?;
        if !result.success {
            return Err(format!(
                "could not authenticate server.describe for {}: {}",
                selected.path.display(),
                result.explanation()
            ));
        }
        let checked = subc_transport::connection_file::read_for_client(&selected.path)
            .map_err(|error| error.to_string())?;
        if selected.info.daemon_id != checked.daemon_id || selected.info.pid != checked.pid {
            return Err(format!(
                "daemon publication {} changed during server.describe",
                selected.path.display()
            ));
        }
        let facts = serde_json::from_str(&result.stdout)
            .map_err(|error| format!("invalid server.describe: {error}"))?;
        Ok(Observation {
            identity: format!("{:02x?}", checked.daemon_id),
            facts,
        })
    }

    fn build(&self, module: Option<&str>, observation: &Observation) -> Result<Build, String> {
        if module.is_none() {
            daemon_build(&observation.facts)
        } else {
            let digest = crate::setup::install_journal::files::digest(self.module_image)?;
            module_build(self.expected_declaration, &observation.facts, digest)
        }
    }

    fn manager(&mut self, program: &str, args: Vec<String>) -> Result<(), String> {
        let result = self.runner.run(program, &args)?;
        if !result.success {
            return Err(format!(
                "{program} {} failed: {}",
                args.join(" "),
                result.explanation()
            ));
        }
        Ok(())
    }
}

impl<R: runtime::CommandRunner> Control for SystemControl<'_, R> {
    fn daemon_live(&mut self) -> Result<bool, String> {
        Ok(runtime::observe(self.platform, self.runner)?.live)
    }
    fn capture(&mut self, module: Option<&str>) -> Result<Prior, String> {
        if module.is_some() {
            return super::capture_with_intent(self, module);
        }
        let status = runtime::observe(self.platform, self.runner)?;
        // The registration, liveness and launchd guard must describe the
        // same moment, not a separate preflight before a crash or backoff.
        super::super::registration::refuse_stopped_launch_agent(self.platform, status)?;
        let live = if status.live {
            let observation = self.observe_running_daemon()?;
            Some(Live {
                build: self.build(None, &observation)?,
                identity: observation.identity,
            })
        } else {
            None
        };
        Ok(Prior {
            live,
            intent: Some(Intent::Daemon {
                registered: status.registered,
            }),
        })
    }
    fn intent(&mut self, module: Option<&str>) -> Result<Option<Intent>, String> {
        match module {
            Some(module) => Ok(self
                .module_enabled(module)?
                .map(|enabled| Intent::Module { enabled })),
            None => Ok(Some(Intent::Daemon {
                registered: runtime::observe(self.platform, self.runner)?.registered,
            })),
        }
    }

    fn restore_daemon_registration(&mut self, registered: bool) -> Result<(), String> {
        let observed = runtime::observe(self.platform, self.runner)?;
        if observed.registered == registered {
            return Ok(());
        }
        let label = self.platform.identifier();
        if !registered {
            return Err(format!("{label} was unregistered before installation, but is registered now; remove the unexpected registration, then rerun setup"));
        }
        match self.platform {
            runtime::RuntimePlatform::Linux => {
                self.manager("systemctl", vec!["--user".into(), "daemon-reload".into()])?;
                systemd_unit::verify_fragment(&self.paths.definition, self.runner)?;
                self.manager(
                    "systemctl",
                    vec!["--user".into(), "enable".into(), label.into()],
                )
            }
            runtime::RuntimePlatform::Windows => {
                runtime::register(self.platform, self.paths, self.runner)
            }
            runtime::RuntimePlatform::Macos if !observed.live => {
                let definition = self.paths.definition.to_string_lossy();
                let quoted = format!("'{}'", definition.replace('\'', "'\"'\"'"));
                crate::setup::messages::warning(format!("{label} was left stopped rather than starting it by re-registering. To register and start it when ready, run `launchctl bootstrap gui/$(id -u) {quoted}`."));
                Ok(())
            }
            runtime::RuntimePlatform::Macos => Err(format!(
                "{label} is unexpectedly live without a registration; stop it, then rerun setup"
            )),
        }
    }
    fn module_supervised(&mut self, module: &str) -> Result<bool, String> {
        Ok(self.module_enabled(module)?.is_some())
    }
    fn live(&mut self, module: Option<&str>) -> Result<Option<Live>, String> {
        let Some(observed) = self.observe(module)? else {
            return Ok(None);
        };
        let build = self.build(module, &observed)?;
        Ok(Some(Live {
            identity: observed.identity,
            build,
        }))
    }

    fn same_process(
        &mut self,
        module: Option<&str>,
        before: Option<&Live>,
    ) -> Result<bool, String> {
        Ok(self
            .observe(module)?
            .as_ref()
            .map(|observed| observed.identity.as_str())
            == before.map(|before| before.identity.as_str()))
    }

    fn matches_process(
        &mut self,
        module: Option<&str>,
        before: Option<&Live>,
        after: Option<&Build>,
    ) -> Result<bool, String> {
        let observed = self.observe(module)?;
        match (observed, after) {
            (None, None) => Ok(true),
            (Some(observed), Some(after))
                if before.is_none_or(|before| before.identity != observed.identity) =>
            {
                Ok(self.build(module, &observed)? == *after)
            }
            _ => Ok(false),
        }
    }

    fn stop(&mut self, module: Option<&str>) -> Result<(), String> {
        if let Some(module) = module {
            return self.ck(&["module", "stop", module]).map(|_| ());
        }
        let unit = self.platform.identifier().to_owned();
        let stopped = match self.platform {
            runtime::RuntimePlatform::Linux => {
                if systemd_unit::loaded_fragment(self.runner)?.is_some() {
                    systemd_unit::verify_fragment(&self.paths.definition, self.runner)?;
                }
                self.manager("systemctl", vec!["--user".into(), "stop".into(), unit])
            }
            runtime::RuntimePlatform::Macos => self.manager(
                "launchctl",
                vec![
                    "bootout".into(),
                    format!("gui/{}/{unit}", runtime::current_uid()),
                ],
            ),
            runtime::RuntimePlatform::Windows => {
                self.manager("schtasks.exe", vec!["/End".into(), "/TN".into(), unit])
            }
        };
        stopped.or_else(|error| {
            if runtime::observe(self.platform, self.runner)
                .is_ok_and(|status| !status.registered && !status.live)
            {
                Ok(())
            } else {
                Err(error)
            }
        })
    }

    fn stop_instruction(&self, module: Option<&str>) -> String {
        if let Some(module) = module {
            return format!("ck module stop {module}");
        }
        let unit = self.platform.identifier();
        match self.platform {
            runtime::RuntimePlatform::Linux => format!("systemctl --user stop {unit}"),
            runtime::RuntimePlatform::Macos => {
                format!("launchctl bootout gui/{}/{unit}", runtime::current_uid())
            }
            runtime::RuntimePlatform::Windows => format!("schtasks.exe /End /TN {unit}"),
        }
    }

    fn start(&mut self, module: Option<&str>) -> Result<(), String> {
        if let Some(module) = module {
            return self.ck(&["module", "start", module]).map(|_| ());
        }
        if self.platform == runtime::RuntimePlatform::Macos
            && !runtime::observe(self.platform, self.runner)?.registered
        {
            return runtime::register(self.platform, self.paths, self.runner);
        }
        runtime::start_installed(self.platform, self.paths, self.runner)
    }

    fn restart(&mut self, module: Option<&str>) -> Result<(), String> {
        if let Some(module) = module {
            return self.ck(&["module", "restart", module]).map(|_| ());
        }
        let old = if self.platform == runtime::RuntimePlatform::Linux {
            None
        } else {
            runtime::connected_daemon(self.runner, &self.paths.daemon, self.connection)?
        };
        runtime::restart_via_service_manager_on(
            self.platform,
            &self.paths.definition,
            self.runner,
            old.as_ref(),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LaunchAgent {
        registered: bool,
        live_prints: usize,
    }

    impl runtime::CommandRunner for LaunchAgent {
        fn run(
            &mut self,
            program: &str,
            args: &[String],
        ) -> Result<runtime::CommandResult, String> {
            assert_eq!(program, "launchctl");
            assert_eq!(
                args.first().map(String::as_str),
                Some("print"),
                "recovery must not load or start the stopped job"
            );
            let stdout = if self.live_prints == 0 {
                "state = waiting"
            } else {
                self.live_prints -= 1;
                "state = running"
            };
            Ok(runtime::CommandResult {
                success: self.registered,
                stdout: stdout.into(),
                ..Default::default()
            })
        }
    }

    #[test]
    fn macos_activation_capture_refuses_a_job_that_stopped_after_preflight() {
        use crate::setup::install_journal::{activation, registration, Journal};
        let root = cortexkit_test_support::ScratchDir::new("macos-activation-preflight-race");
        let platform = runtime::RuntimePlatform::Macos;
        let paths = runtime::runtime_paths(platform, &root.join("bin"), root.path());
        let mut manager = LaunchAgent {
            registered: true,
            live_prints: 2,
        };
        let preflight = runtime::observe(platform, &mut manager).unwrap();
        assert!(preflight.registered && preflight.live);
        registration::refuse_stopped_launch_agent(platform, preflight).unwrap();
        let mut journal = Journal::create(&root.join("manifest")).unwrap();
        let mut control = SystemControl {
            executable: Path::new("ck"),
            connection: None,
            platform,
            paths: &paths,
            module_image: &paths.daemon,
            expected_declaration: None,
            runner: &mut manager,
        };
        let error = activation::prepare(
            &mut journal,
            &paths.daemon,
            None,
            Some(Build::Image {
                sha256: "candidate".into(),
            }),
            &mut control,
        )
        .unwrap_err();
        assert!(
            error.contains(r#"launchctl kickstart "gui/$(id -u)/cortexkit.subc""#),
            "{error}"
        );
        assert_eq!(journal.next_index(), 0, "no activation was written ahead");
    }

    #[test]
    fn macos_activation_undo_accepts_a_stopped_unregistered_job_without_starting_it() {
        use crate::setup::install_journal::{activation, Action, Step, Undo};
        let root = cortexkit_test_support::ScratchDir::new("macos-activation-stopped-intent");
        let platform = runtime::RuntimePlatform::Macos;
        let paths = runtime::runtime_paths(platform, &root.join("bin"), root.path());
        let mut manager = LaunchAgent {
            registered: false,
            live_prints: 0,
        };
        let mut control = SystemControl {
            executable: Path::new("ck"),
            connection: None,
            platform,
            paths: &paths,
            module_image: &paths.daemon,
            expected_declaration: None,
            runner: &mut manager,
        };
        let step = Step {
            target: paths.daemon.clone(),
            action: Action::Activation {
                module: None,
                before: None,
                after: None,
                before_intent: Some(Intent::Daemon { registered: true }),
            },
        };
        let (result, warnings) = crate::setup::messages::keep(|| {
            activation::undo(&step, Undo::Reactivate, &mut control)
        });
        result.unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("launchctl bootstrap gui/$(id -u)"));
        assert!(warnings[0].contains(paths.definition.to_str().unwrap()));
        assert!(!manager.registered);
    }

    fn module_recovery_when_daemon_down(
        phase: crate::setup::install_journal::Undo,
    ) -> Result<(), String> {
        use crate::setup::install_journal::{activation, Action, Step};
        struct Offline;
        impl runtime::CommandRunner for Offline {
            fn run(
                &mut self,
                program: &str,
                _: &[String],
            ) -> Result<runtime::CommandResult, String> {
                Ok(runtime::CommandResult {
                    stderr: if program == "ck" {
                        "no connection to daemon".into()
                    } else {
                        String::new()
                    },
                    ..runtime::CommandResult::default()
                })
            }
        }
        let fixture = cortexkit_test_support::ScratchDir::new("module-recovery-daemon-down");
        let platform = runtime::RuntimePlatform::current();
        let paths = runtime::runtime_paths(platform, &fixture.join("bin"), fixture.path());
        let path = fixture.join("module");
        let mut control = SystemControl {
            executable: Path::new("ck"),
            connection: None,
            platform,
            paths: &paths,
            module_image: &path,
            expected_declaration: None,
            runner: &mut Offline,
        };
        let step = Step {
            target: path.clone(),
            action: Action::Activation {
                module: Some("backoff".into()),
                before: None,
                after: None,
                before_intent: Some(Intent::Module { enabled: true }),
            },
        };
        activation::undo(&step, phase, &mut control)
    }

    #[test]
    fn module_recovery_when_daemon_down_can_quiesce() {
        module_recovery_when_daemon_down(crate::setup::install_journal::Undo::Quiesce).unwrap();
    }

    #[test]
    fn module_recovery_when_daemon_down_explains_how_to_reactivate() {
        let error =
            module_recovery_when_daemon_down(crate::setup::install_journal::Undo::Reactivate)
                .unwrap_err();
        assert!(error.contains("backoff"), "{error}");
        assert!(
            error.contains("start the daemon, then rerun the installer command"),
            "{error}"
        );
    }

    #[test]
    fn quiesce_does_not_send_stop_for_an_unsupervised_module() {
        use crate::setup::install_journal::{activation, Action, Step, Undo};

        #[derive(Default)]
        struct Manager {
            stop_calls: usize,
        }
        impl runtime::CommandRunner for Manager {
            fn run(
                &mut self,
                program: &str,
                args: &[String],
            ) -> Result<runtime::CommandResult, String> {
                if program != "ck" {
                    return Ok(runtime::CommandResult {
                        success: true,
                        stdout: "state = running\nStatus: Running".into(),
                        ..Default::default()
                    });
                }
                if args == ["--json", "module", "list"] {
                    return Ok(runtime::CommandResult {
                        success: true,
                        stdout: r#"{"modules":[]}"#.into(),
                        ..Default::default()
                    });
                }
                if args == ["module", "stop", "missing"] {
                    self.stop_calls += 1;
                    return Ok(runtime::CommandResult {
                        success: false,
                        stderr: "unknown_module".into(),
                        ..Default::default()
                    });
                }
                panic!("unexpected command {args:?}");
            }
        }
        let scratch = cortexkit_test_support::ScratchDir::new("journal-unsupervised");
        let target = scratch.join("missing");
        let paths = runtime::runtime_paths(
            runtime::RuntimePlatform::current(),
            scratch.path(),
            scratch.path(),
        );
        let mut manager = Manager::default();
        let mut control = SystemControl {
            executable: Path::new("ck"),
            connection: None,
            platform: runtime::RuntimePlatform::current(),
            paths: &paths,
            module_image: &target,
            expected_declaration: None,
            runner: &mut manager,
        };
        let step = Step {
            target: target.clone(),
            action: Action::Activation {
                module: Some("missing".into()),
                before: None,
                after: None,
                before_intent: None,
            },
        };
        activation::undo(&step, Undo::Quiesce, &mut control).unwrap();
        activation::undo(&step, Undo::Reactivate, &mut control).unwrap();
        assert_eq!(manager.stop_calls, 0);
    }

    struct Probe;

    impl runtime::CommandRunner for Probe {
        fn run(
            &mut self,
            program: &str,
            args: &[String],
        ) -> Result<runtime::CommandResult, String> {
            assert_eq!(program, "ck-probe");
            assert_eq!(
                args,
                ["--subc", "selected.json", "--json", "provenance", "alpha"]
            );
            Ok(runtime::CommandResult {
                success: true,
                stdout: serde_json::json!({
                    "modules": [{ "module_id": "alpha", "daemon_observed": {
                        "pid": 772, "spawned_at_ms": 42, "running_image": { "status": "match" }
                    }}]
                })
                .to_string(),
                ..runtime::CommandResult::default()
            })
        }
    }

    #[test]
    fn module_identity_uses_authenticated_pid_time_and_image_agreement() {
        let fixture = cortexkit_test_support::ScratchDir::new("journal-module-proof");
        let target = fixture.path().join("alpha");
        std::fs::write(&target, b"release image").unwrap();
        let paths = runtime::RuntimePaths {
            definition: fixture.path().join("definition"),
            daemon: fixture.path().join("daemon"),
            config_store_root: fixture.path().to_owned(),
        };
        let mut system = SystemControl {
            executable: Path::new("ck-probe"),
            connection: Some(Path::new("selected.json")),
            platform: runtime::RuntimePlatform::current(),
            paths: &paths,
            module_image: &target,
            expected_declaration: None,
            runner: &mut Probe,
        };
        let live = system.live(Some("alpha")).unwrap().unwrap();
        assert_eq!(live.identity, "772:42");
        assert_eq!(
            live.build,
            Build::Image {
                sha256: crate::setup::install_journal::files::digest(&target)
                    .unwrap()
                    .unwrap()
            }
        );
        assert!(!system
            .matches_process(Some("alpha"), Some(&live), Some(&live.build))
            .unwrap());
    }
}
