// These tests need Darwin's real responsibility ledger, not a mock of spawn.
// Keep each test's name visible (ignored) on other systems.

#[cfg(target_os = "macos")]
mod macos {
    use serde_json::{json, Value};
    use std::{
        fs,
        os::unix::process::CommandExt,
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    use subc_test_support::TestTempDir;

    pub struct Fixture {
        pub root: TestTempDir,
        child: Child,
    }

    impl Fixture {
        pub fn boot(wire: bool, hooks: bool, extra_env: Value) -> Self {
            Self::boot_with_daemon_env(wire, hooks, extra_env, json!({}))
        }

        pub fn boot_with_daemon_env(
            wire: bool,
            hooks: bool,
            extra_env: Value,
            daemon_env: Value,
        ) -> Self {
            let root = TestTempDir::new("privacy-identity");
            for directory in ["config/cortexkit", "runtime", "data/cortexkit/run"] {
                fs::create_dir_all(root.join(directory)).unwrap();
            }
            let mut env = json!({"FAKE_AFT_MODULE_ID":"privacy-stub", "FAKE_AFT_PRIVACY_REPORT":root.join("observation.json")});
            if !wire {
                env["FAKE_AFT_NEVER_CONNECT"] = json!("1");
            }
            env.as_object_mut()
                .unwrap()
                .extend(extra_env.as_object().unwrap().clone());
            let config = json!({"version":1, "modules":{"privacy-stub":{
                "program":env!("CARGO_BIN_EXE_fake-aft-stub"),
                "reserved":wire,
                "protocol":if wire { "subc" } else { "none" }, "overlap":"safe",
                "restart":{"max_restarts":0}, "env":env
            }}});
            fs::write(root.join("config/cortexkit/subc.jsonc"), config.to_string()).unwrap();
            let program = if hooks {
                env!("CARGO_BIN_EXE_ck-subc-under-test")
            } else {
                env!("CARGO_BIN_EXE_ck-subc")
            };
            let mut command = Command::new(program);
            for (key, value) in daemon_env.as_object().unwrap() {
                command.env(key, value.as_str().unwrap());
            }
            let child = command
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_RUNTIME_DIR", root.join("runtime"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("SUBC_PORT", "0")
                .env("SUBC_CGROUP_PLACEMENT", "disabled")
                .env_remove("CK_LOG")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap();
            let fixture = Self { root, child };
            fixture.wait(|| {
                fixture
                    .root
                    .join("runtime")
                    .join(subc_transport::CONNECTION_FILE_NAME)
                    .exists()
            });
            fixture
        }

        pub fn wait(&self, mut predicate: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !predicate() {
                assert!(
                    Instant::now() < deadline,
                    "privacy fixture timed out; daemon log: {}",
                    self.log()
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        pub fn log(&self) -> String {
            fs::read_dir(self.root.join("data/cortexkit/run/logs"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("subc."))
                .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
                .collect()
        }

        pub fn observation(&self) -> Value {
            self.wait(|| self.root.join("observation.json").exists());
            serde_json::from_slice(&fs::read(self.root.join("observation.json")).unwrap()).unwrap()
        }

        pub fn ck(&self, args: &[&str]) -> std::process::Output {
            Command::new(env!("CARGO_BIN_EXE_ck"))
                .arg("--subc")
                .arg(
                    self.root
                        .join("runtime")
                        .join(subc_transport::CONNECTION_FILE_NAME),
                )
                .args(args)
                .env("XDG_CONFIG_HOME", self.root.join("config"))
                .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
                .env("XDG_DATA_HOME", self.root.join("data"))
                .output()
                .unwrap()
        }

        pub fn status(&self) -> Value {
            let output = self.ck(&["module", "status", "privacy-stub", "--json"]);
            serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
        }

        pub fn roster(&self) -> Value {
            serde_json::from_slice(
                &fs::read(self.root.join("data/cortexkit/run/live-children.json")).unwrap(),
            )
            .unwrap()
        }

        pub fn daemon_pid(&self) -> u32 {
            self.child.id()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let pid = rustix::process::Pid::from_raw(self.child.id() as i32).unwrap();
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    pub fn own_identity() {
        let fixture = Fixture::boot(true, false, json!({}));
        let report = fixture.observation();
        assert_eq!(report["responsible_pid"], report["pid"]);
        assert_eq!(report["parent_pid"], fixture.daemon_pid());
        fixture.wait(|| {
            fixture
                .log()
                .contains("module spawned with own privacy identity (responsibility disclaimed)")
        });
    }

    pub fn direct_control() {
        let root = TestTempDir::new("privacy-direct-control");
        let report = root.join("observation.json");
        let mut child = Command::new(env!("CARGO_BIN_EXE_fake-aft-stub"))
            .env("FAKE_AFT_PRIVACY_REPORT", &report)
            .env("FAKE_AFT_EXIT_CODE", "0")
            .env_remove("SUBC_LAUNCH_NONCE_FD")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_DATA_HOME", root.join("data"))
            .spawn()
            .unwrap();
        assert!(child.wait().unwrap().success());
        let observation: Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        assert_ne!(observation["pid"], observation["responsible_pid"]);
        assert_eq!(
            observation["responsible_pid"],
            observation["parent_responsible_pid"]
        );
    }

    pub fn nonce_and_group() {
        let fixture = Fixture::boot(true, false, json!({}));
        let report = fixture.observation();
        assert_eq!(report["nonce_source"], "fd");
        assert_eq!(report["process_group"], report["pid"]);
        fixture.wait(|| fixture.status()["module"]["live"] == true);
    }

    pub fn fail_closed() {
        let fixture = Fixture::boot(true, true, json!({"SUBC_TEST_PRIVACY_MISSING_SYMBOL":"1"}));
        fixture.wait(|| {
            fixture.root.join("observation.json").exists()
                || fixture.status()["module"]["state"] == "failed"
        });
        assert!(
            !fixture.root.join("observation.json").exists(),
            "missing symbol must never exec the stub"
        );
        assert!(
            fixture
                .log()
                .contains("responsibility_spawnattrs_setdisclaim is unavailable"),
            "{}",
            fixture.log()
        );
        let terminals = fixture.ck(&["module", "terminals", "privacy-stub", "--json"]);
        assert!(String::from_utf8_lossy(&terminals.stdout)
            .contains("responsibility_spawnattrs_setdisclaim is unavailable"));
    }

    pub fn confirmed_roster() {
        let fixture = Fixture::boot(
            true,
            true,
            json!({"SUBC_TEST_PRIVACY_EXEC_DELAY_MS":"1200"}),
        );
        let initial = fixture.roster();
        assert!(
            initial["children"][0]["executable"].is_null(),
            "unconfirmed image must never name ck-subc: {initial}"
        );
        let report = fixture.observation();
        fixture.wait(|| fixture.roster()["children"][0]["executable"].is_object());
        let recorded = fixture.roster();
        let expected =
            subc_os::file_identity(std::path::Path::new(env!("CARGO_BIN_EXE_fake-aft-stub")))
                .unwrap();
        assert_eq!(recorded["children"][0]["pid"], report["pid"]);
        assert_eq!(
            recorded["children"][0]["executable"]["device"],
            expected.device
        );
        assert_eq!(
            recorded["children"][0]["executable"]["inode"],
            expected.inode
        );
    }

    pub fn no_protocol() {
        let fixture = Fixture::boot(false, false, json!({}));
        let report = fixture.observation();
        assert_eq!(report["pid"], report["responsible_pid"]);
        assert!(report["nonce_source"].is_null());
        assert_eq!(report["process_group"], report["pid"]);
    }

    pub fn production_ignores_fault_injection() {
        let fixture = Fixture::boot(true, false, json!({"SUBC_TEST_PRIVACY_MISSING_SYMBOL":"1"}));
        let report = fixture.observation();
        assert_eq!(report["pid"], report["responsible_pid"]);
        fixture.wait(|| fixture.status()["module"]["live"] == true);
    }

    pub fn swap() {
        // A swap uses the same trampoline and launch-nonce delivery (an inherited fd-3 pipe) as a boot.
        let fixture = Fixture::boot(true, false, json!({}));
        let old_pid = fixture.observation()["pid"].clone();
        fixture.wait(|| fixture.status()["module"]["live"] == true);
        let output = fixture.ck(&["module", "restart", "privacy-stub", "--swap"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = fixture.observation();
        assert_ne!(report["pid"], old_pid);
        assert_eq!(report["pid"], report["responsible_pid"]);
        assert_eq!(report["nonce_source"], "fd");
        assert_eq!(report["pid"], report["process_group"]);
    }

    pub fn startup_refusal() {
        let fixture = Fixture::boot_with_daemon_env(
            true,
            true,
            json!({}),
            json!({"SUBC_TEST_PRIVACY_MISSING_SYMBOL":"1"}),
        );
        fixture.wait(|| fixture.status()["module"]["state"] == "failed");
        assert!(!fixture.root.join("observation.json").exists());
        let log = fixture.log();
        assert_eq!(
            log.lines()
                .filter(|line| line.contains(
                    "macOS privacy identity unavailable; supervised launches will refuse"
                ))
                .count(),
            1,
            "{log}"
        );
        assert!(
            log.contains("responsibility_spawnattrs_setdisclaim is unavailable"),
            "{log}"
        );
        assert!(
            fixture.ck(&["daemon", "--json"]).status.success(),
            "daemon must keep serving control requests"
        );
    }

    pub fn ack_timeout() {
        let fixture = Fixture::boot(
            true,
            true,
            json!({"SUBC_TEST_PRIVACY_EXEC_DELAY_MS":"6000"}),
        );
        fixture.wait(|| fixture.status()["module"]["state"] == "failed");
        assert!(!fixture.root.join("observation.json").exists());
        assert!(fixture
            .log()
            .contains("privacy identity trampoline did not exec within 5s"));
        let terminals = fixture.ck(&["module", "terminals", "privacy-stub", "--json"]);
        assert!(String::from_utf8_lossy(&terminals.stdout)
            .contains("privacy identity trampoline did not exec within 5s"));
    }

    pub fn trampoline_image_refused() {
        let fixture = Fixture::boot(
            true,
            true,
            json!({"SUBC_TEST_PRIVACY_CLOSE_ACK_WITHOUT_EXEC":"1"}),
        );
        fixture.wait(|| fixture.status()["module"]["state"] == "failed");
        assert!(!fixture.root.join("observation.json").exists());
        assert!(fixture
            .log()
            .contains("privacy identity trampoline executable mismatch"));
        assert!(fixture.roster()["children"].as_array().unwrap().is_empty());
    }

    pub fn module_exit_121() {
        let fixture = Fixture::boot(true, false, json!({"FAKE_AFT_EXIT_CODE":"121"}));
        let report = fixture.observation();
        assert_eq!(report["responsible_pid"], report["pid"]);
        fixture.wait(|| fixture.status()["module"]["state"] == "failed");
        assert_eq!(fixture.status()["module"]["last_exit_code"], 121);
        let terminals = fixture.ck(&["module", "terminals", "privacy-stub", "--json"]);
        let history = String::from_utf8_lossy(&terminals.stdout);
        assert!(history.contains("crash budget exhausted"), "{history}");
        assert!(
            !history.contains("privacy identity"),
            "module exit 121 is not a trampoline refusal: {history}"
        );
        assert!(
            !history.contains("responsibility_spawnattrs_setdisclaim"),
            "module exit 121 is not a trampoline refusal: {history}"
        );
        assert!(!fixture
            .log()
            .contains("privacy identity trampoline refused module spawn"));
    }
}

macro_rules! macos_test {
    ($name:ident, $body:ident) => {
        #[test]
        #[cfg_attr(
            not(target_os = "macos"),
            ignore = "requires macOS responsibility ledger"
        )]
        fn $name() {
            #[cfg(target_os = "macos")]
            macos::$body();
        }
    };
}

macos_test!(macos_module_is_its_own_responsible_process, own_identity);
macos_test!(
    macos_direct_spawn_inherits_responsibility_control,
    direct_control
);
macos_test!(
    macos_disclaimed_module_registers_with_fd_nonce_and_own_group,
    nonce_and_group
);
macos_test!(macos_missing_symbol_fails_closed_without_exec, fail_closed);
macos_test!(
    macos_roster_records_only_confirmed_module_image,
    confirmed_roster
);
macos_test!(macos_protocol_none_is_also_disclaimed, no_protocol);
macos_test!(
    macos_production_config_cannot_force_symbol_failure,
    production_ignores_fault_injection
);
macos_test!(macos_swap_candidate_has_own_identity_and_fd_nonce, swap);
macos_test!(
    macos_startup_symbol_failure_refuses_launch_but_keeps_control_server,
    startup_refusal
);
macos_test!(
    macos_exec_ack_timeout_is_named_and_never_admits_an_image,
    ack_timeout
);
macos_test!(
    macos_trampoline_image_is_never_accepted_as_module,
    trampoline_image_refused
);
macos_test!(
    macos_exec_success_with_immediate_exit_121_is_not_a_trampoline_refusal,
    module_exit_121
);
