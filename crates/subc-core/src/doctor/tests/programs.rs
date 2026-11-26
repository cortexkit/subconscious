use super::*;
use crate::doctor::{
    host::SetEnabledError, program_checks::ProgramConfig, run::run_with_programs_on,
};
use std::collections::VecDeque;

const STOP: &str = "stop";
const START: &str = "start";
const LIST: &str = "list";

type Mutation = dyn Fn(&str, bool, Instant) -> Result<bool, SetEnabledError> + Send + Sync;

#[derive(Default)]
pub(super) struct DaemonScript {
    lists: Mutex<VecDeque<RpcResult<Vec<SupervisorEntry>>>>,
    change: Option<Box<Mutation>>,
}

impl DaemonScript {
    pub(super) fn next_list(&self) -> Option<RpcResult<Vec<SupervisorEntry>>> {
        self.lists.lock().unwrap().pop_front()
    }

    pub(super) fn set_enabled(
        &self,
        module: &str,
        enabled: bool,
        deadline: Instant,
    ) -> Result<bool, SetEnabledError> {
        self.change.as_ref().expect("unexpected lifecycle call")(module, enabled, deadline)
    }
}

struct Prompt<F>(F);
impl<F: Fn(&str) -> bool> Prompter for Prompt<F> {
    fn interactive(&self) -> bool {
        true
    }
    fn confirm(&self, module: &str, _: &cortexkit_doctor::Finding) -> bool {
        self.0(module)
    }
}

struct NoTerminal;
impl Prompter for NoTerminal {
    fn interactive(&self) -> bool {
        false
    }
    fn confirm(&self, _: &str, _: &cortexkit_doctor::Finding) -> bool {
        panic!("non-interactive run must not prompt")
    }
}

struct Rig {
    host: Arc<Fixture>,
    entries: BTreeMap<String, Value>,
    config: ProgramConfig,
    scenarios: PathBuf,
    interrupt: Arc<Interrupt>,
}

impl Rig {
    fn new() -> Self {
        let host = Fixture::new("doctor-program");
        let scenarios = host.data.join("doctor-fixture");
        fs::create_dir(&scenarios).unwrap();
        let config = ProgramConfig {
            path: host.scratch.join("subc.jsonc"),
            runner: cortexkit_doctor::runner::Options::new()
                .with_data_home(&host.data)
                .with_config_home(host.scratch.path())
                .with_force_setsid(true),
            ..ProgramConfig::default()
        };
        Self {
            host: Arc::new(host),
            entries: BTreeMap::new(),
            config,
            scenarios,
            interrupt: Arc::new(Interrupt::default()),
        }
    }

    fn host_mut(&mut self) -> &mut Fixture {
        Arc::get_mut(&mut self.host).unwrap()
    }

    fn program(&mut self, id: &str, scenario: Value) -> PathBuf {
        let fixture = std::env::var_os("CK_DOCTOR_TEST_FIXTURE").map_or_else(
            || cortexkit_test_support::cargo_profile_bin("doctor-module-fixture"),
            PathBuf::from,
        );
        assert!(fixture.is_file(), "build doctor-module-fixture with `cargo build -p subc-core --features test-support --bin doctor-module-fixture`; fixture not found at {}", fixture.display());
        let path = self.host.scratch.join(id);
        fs::hard_link(&fixture, &path)
            .or_else(|_| fs::copy(&fixture, &path).map(|_| ()))
            .unwrap();
        fs::write(
            self.scenarios.join(format!("{id}.json")),
            scenario.to_string(),
        )
        .unwrap();
        // The configured arguments must never reach the contract's check argv.
        self.entries.insert(
            id.into(),
            json!({"program": path, "args": ["--unrelated"], "enabled": false}),
        );
        path
    }

    fn marker(&self, id: &str) -> PathBuf {
        self.scenarios.join(format!("{id}.fixed"))
    }

    fn write_config(&self) {
        fs::write(
            &self.config.path,
            format!(
                "// local configuration\n{}",
                json!({"version":1, "modules": self.entries})
            ),
        )
        .unwrap();
    }

    fn run(&self, fix: bool, prompter: &dyn Prompter) -> ProgramRun {
        self.run_with_options(
            DoctorOptions {
                fix,
                connection_override: None,
            },
            prompter,
        )
    }

    fn run_with_options(&self, options: DoctorOptions, prompter: &dyn Prompter) -> ProgramRun {
        run_with_programs_on(
            self.host.clone(),
            options,
            Programs {
                prompter,
                interrupt: &self.interrupt,
                operator_budget: Duration::from_millis(750),
            },
            Arc::new(FakeLogs::default()),
            &short_limits(),
            &self.config,
        )
    }

    fn script(
        &mut self,
        lists: impl IntoIterator<Item = RpcResult<Vec<SupervisorEntry>>>,
        change: impl Fn(&str, bool, Instant) -> Result<bool, SetEnabledError> + Send + Sync + 'static,
    ) {
        self.host_mut().set_daemon(daemon(lists, change));
    }

    fn offer_revert(&mut self) {
        let row = self
            .host
            .add_window(0, 1, WindowPhase::Update(Liveness::Gone));
        self.host_mut().windows.get_mut().unwrap().push(row);
    }
}

fn daemon(
    lists: impl IntoIterator<Item = RpcResult<Vec<SupervisorEntry>>>,
    change: impl Fn(&str, bool, Instant) -> Result<bool, SetEnabledError> + Send + Sync + 'static,
) -> FakeDaemon {
    FakeDaemon {
        program_script: DaemonScript {
            lists: Mutex::new(lists.into_iter().collect()),
            change: Some(Box::new(change)),
        },
        ..FakeDaemon::default()
    }
}

fn entry(id: &str, enabled: bool, state: &str) -> SupervisorEntry {
    SupervisorEntry::new(
        id,
        state,
        enabled,
        enabled && state == "running",
        Health::Unknown,
    )
    .with_drain_timeout_ms(Some(0))
}

fn calls(daemon: &FakeDaemon) -> Vec<(&'static str, String)> {
    daemon
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(call, _, _)| matches!(*call, LIST | START | STOP))
        .map(|(call, id, _)| (*call, id.clone()))
        .collect()
}

fn actions(daemon: &FakeDaemon) -> Vec<&'static str> {
    calls(daemon).into_iter().map(|(call, _)| call).collect()
}

fn first_fix(run: &ProgramRun) -> &ModuleFix {
    &run.run.document.fix.as_ref().unwrap().module_fixes[0]
}

fn result(row: &ModuleFix) -> ModuleFixOutcome {
    row.result.as_ref().unwrap().outcome
}

fn replace_file(path: &Path) {
    // Keep the old file alive while installing a different inode at its name.
    fs::rename(path, path.with_extension("old")).unwrap();
    fs::write(path, "a different file").unwrap();
}

#[test]
fn classifications_keep_daemon_and_program_findings_separate() {
    let mut rig = Rig::new();
    let a = rig.program("a", json!({}));
    rig.program("b", json!({"check":"clean"}));
    rig.program("c", json!({"check":"unknown"}));
    rig.entries
        .insert("d".into(), json!({"program":a, "protocol":"none"}));
    rig.entries.insert(
        "e".into(),
        json!({"program":rig.host.scratch.join("missing")}),
    );
    rig.entries.insert(
        "f".into(),
        json!({"script":{"runtime":"bun","path":"bundle.js"}}),
    );
    rig.entries
        .insert("g".into(), json!({"program":"search-path"}));
    rig.write_config();
    rig.host_mut().set_daemon(FakeDaemon {
        modules: vec![entry("a", true, "failed")],
        ..FakeDaemon::default()
    });
    let before = fs::read(&rig.config.path).unwrap();
    let run = rig.run(false, &NoTerminal);
    let doc = serde_json::to_value(&run.run.document).unwrap();
    assert_eq!(
        doc["modules"],
        json!([
            {"id":"a","outcome":"findings","findings":1,"detail":null},
            {"id":"b","outcome":"clean","findings":0,"detail":null},
            {"id":"c","outcome":"no_checks","findings":0,"detail":null},
            {"id":"d","outcome":"not_asked","findings":0,"detail":"protocol_none"},
            {"id":"e","outcome":"spawn_failed","findings":0,"detail":"not_found"},
            {"id":"f","outcome":"not_asked","findings":0,"detail":"script"},
            {"id":"g","outcome":"not_asked","findings":0,"detail":"program_not_absolute"},
        ])
    );
    assert_eq!(
        run.checked
            .iter()
            .map(|row| row.module_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(run.checked[0].unchanged());
    let findings = doc["findings"].as_array().unwrap();
    let same_id = findings
        .iter()
        .filter(|row| row["id"] == "module.failed")
        .collect::<Vec<_>>();
    assert_eq!(same_id.len(), 2);
    assert_eq!(same_id[0]["layer"], "module");
    assert!(same_id[0].get("fix").is_none());
    assert_eq!(same_id[1]["layer"], "module_program");
    assert_eq!(same_id[1]["owner"], "a");
    assert_eq!(
        same_id[1]["evidence"],
        json!([{"name":"attempts","kind":"count","value":3}])
    );
    assert_eq!(same_id[1]["fix"]["plan"]["timeout_ms"], 4_000);
    assert_eq!(same_id[1]["fix"]["class"], "safe");
    assert_eq!(fs::read(&rig.config.path).unwrap(), before);
    assert!(!rig.marker("a").exists());
}

#[test]
fn launch_and_usage_exit_classifications_use_runner_results() {
    use std::os::unix::fs::PermissionsExt;

    let mut rig = Rig::new();
    let interpreter = rig.host.scratch.join("missing-interpreter");
    for (id, script, mode) in [
        (
            "exit-two-with-output",
            "#!/bin/sh\nprintf 'check failed'; exit 2\n".to_owned(),
            0o700,
        ),
        (
            "missing-interpreter",
            format!("#!{}\n", interpreter.display()),
            0o700,
        ),
        ("not-executable", "#!/bin/sh\nexit 0\n".to_owned(), 0o600),
    ] {
        let path = rig.host.scratch.join(format!("program-{id}"));
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        rig.entries.insert(id.into(), json!({"program":path}));
    }
    rig.write_config();
    let run = rig.run(false, &NoTerminal);
    let doc = serde_json::to_value(&run.run.document).unwrap();
    assert_eq!(
        doc["modules"],
        json!([
            {"id":"exit-two-with-output","outcome":"crashed","findings":0,"detail":"exit:2"},
            {"id":"missing-interpreter","outcome":"spawn_failed","findings":0,"detail":"not_found"},
            {"id":"not-executable","outcome":"spawn_failed","findings":0,"detail":"permission_denied"},
        ])
    );
}

#[test]
fn missing_identity_rejects_success_but_preserves_launch_errors() {
    let mut rig = Rig::new();
    rig.config.identity_lookup = |_| None;
    rig.program("checked", json!({}));
    rig.entries.insert(
        "missing".into(),
        json!({"program": rig.host.scratch.join("missing")}),
    );
    rig.write_config();

    let run = rig.run(
        true,
        &Prompt(|_: &str| panic!("a check without a file identity must not offer a repair")),
    );
    let doc = serde_json::to_value(&run.run.document).unwrap();
    assert_eq!(
        doc["modules"],
        json!([
            {"id":"checked","outcome":"invalid_output","findings":0,"detail":"identity_unreadable"},
            {"id":"missing","outcome":"spawn_failed","findings":0,"detail":"not_found"},
        ])
    );
    let problem = finding(&run.run, "module_program.identity_unreadable");
    assert!(problem.summary.contains("file identity could not be read"));
    assert_eq!(problem.evidence["detail"], "identity_unreadable");
    assert!(!ids(&run.run).contains(&"module.failed"));
    assert!(run.checked.is_empty());
}

#[test]
fn config_errors_and_override_do_not_run_programs() {
    let rig = Rig::new();
    assert!(rig.run(false, &NoTerminal).checked.is_empty());
    for (text, expected) in [
        ("/*", "invalid_jsonc"),
        (
            r#"{"version":1,"modules":{"subc":{"program":"/unused"}}}"#,
            "invalid_value",
        ),
    ] {
        fs::write(&rig.config.path, text).unwrap();
        let report = rig.run(false, &NoTerminal).run;
        assert!(report.document.modules.as_ref().unwrap().is_empty());
        assert_eq!(
            finding(&report, "module_program.config_unreadable").evidence,
            json!({"error":expected})
        );
    }
    let report = rig
        .run_with_options(
            DoctorOptions {
                fix: false,
                connection_override: Some("named.json".into()),
            },
            &NoTerminal,
        )
        .run;
    assert!(report.document.modules.as_ref().unwrap().is_empty());
    assert_eq!(
        finding(&report, "module_program.connection_override").severity,
        Severity::Info
    );
    assert!(!ids(&report).contains(&"module_program.config_unreadable"));
    let stack = run_with(
        rig.host.clone(),
        DoctorOptions::default(),
        Arc::new(FakeLogs::default()),
        &short_limits(),
    );
    assert!(serde_json::to_value(stack.document)
        .unwrap()
        .get("modules")
        .is_none());
}

#[test]
fn program_errors_affect_status_but_runner_errors_hide_output() {
    let mut rig = Rig::new();
    rig.config.limits.check = Duration::from_millis(300);
    rig.config.limits.term_to_kill = Duration::from_millis(10);
    for (i, check) in ["crash", "invalid", "oversize", "sleep"]
        .into_iter()
        .enumerate()
    {
        rig.program(&format!("m{i}"), json!({"check":check}));
    }
    rig.write_config();
    let run = rig.run(false, &NoTerminal).run;
    assert_eq!(run.exit_code, 0);
    assert_eq!(
        finding(&run, "module_program.crashed").evidence["detail"],
        "exit:41"
    );
    assert_eq!(
        finding(&run, "module_program.invalid_output").evidence["detail"],
        "not_json"
    );
    assert_eq!(
        run.document.modules.as_ref().unwrap()[2].detail.as_deref(),
        Some("oversize")
    );
    assert!(ids(&run).contains(&"module_program.timed_out"));
    assert!(!serde_json::to_string(&run.document)
        .unwrap()
        .contains("discarded fixture"));
    rig.program("error", json!({"severity":"error"}));
    rig.write_config();
    let run = rig.run(false, &NoTerminal).run;
    assert_eq!(run.exit_code, 1);
    assert!(!run.document.ok);
}

#[test]
fn program_pool_admits_two_full_waves_and_keeps_fast_jobs() {
    for count in [4, 8, 9, 12] {
        let mut rig = Rig::new();
        rig.config.limits.check = Duration::from_millis(450);
        rig.config.limits.term_to_kill = Duration::from_millis(10);
        rig.config.admission = Duration::from_millis(750);
        for i in 0..count {
            rig.program(
                &format!("m{i:02}"),
                json!({"check":if count == 12 { "quick" } else { "sleep" }}),
            );
        }
        rig.write_config();
        let began = Instant::now();
        let run = rig.run(false, &NoTerminal);
        let modules = run.run.document.modules.unwrap();
        assert_eq!(modules.len(), count);
        assert!(began.elapsed() < Duration::from_secs(3));
        for (index, row) in modules.into_iter().enumerate() {
            assert_eq!(row.id, format!("m{index:02}"));
            assert_eq!(
                row.outcome,
                if count == 12 {
                    ModuleOutcome::Clean
                } else if index == 8 {
                    ModuleOutcome::NotStarted
                } else {
                    ModuleOutcome::TimedOut
                }
            );
        }
    }
}

#[test]
fn fix_consent_refusal_or_result_controls_second_pass() {
    for (scenario, consent, decision, outcome, code, passes, exit) in [
        ("fixed", false, "declined", Value::Null, Value::Null, 1, 0),
        ("fixed", true, "applied", json!("fixed"), Value::Null, 2, 0),
        (
            "invalid",
            true,
            "applied",
            json!("no_result"),
            json!("invalid_output"),
            2,
            1,
        ),
        (
            "module_running",
            true,
            "applied",
            json!("refused"),
            json!("module_running"),
            1,
            0,
        ),
        (
            "failed",
            true,
            "applied",
            json!("failed"),
            json!("repair_failed"),
            2,
            1,
        ),
    ] {
        let mut rig = Rig::new();
        rig.program("m", json!({"fix": scenario, "clean_after_fix": true}));
        rig.write_config();
        let run = rig.run(true, &Prompt(|_: &str| consent));
        let doc = serde_json::to_value(&run.run.document).unwrap();
        let fix = &doc["fix"]["module_fixes"][0];
        let result = if outcome.is_null() {
            Value::Null
        } else {
            json!({"outcome": outcome, "code": code, "evidence": null})
        };
        assert_eq!(
            fix,
            &json!({
                "module":"m", "finding":"module.failed", "class":"safe", "decision":decision,
                "skip_reason":null, "result":result, "restart":null,
            }),
            "{scenario}"
        );
        assert_eq!(rig.host.connects.load(Ordering::Relaxed), passes);
        assert_eq!(
            doc["modules"][0]["outcome"],
            if passes == 1 { "findings" } else { "clean" }
        );
        assert_eq!(run.run.exit_code, exit);
        assert_eq!(rig.marker("m").exists(), consent);
    }
}

#[test]
fn fix_orders_safe_before_risky_and_never_asks_without_a_terminal() {
    let mut rig = Rig::new();
    rig.program("a", json!({"risky": true}));
    rig.program("b", json!({}));
    rig.program("c", json!({}));
    rig.write_config();
    let order = Mutex::new(Vec::new());
    let run = rig.run(
        true,
        &Prompt(|id: &str| {
            order.lock().unwrap().push(id.to_owned());
            false
        }),
    );
    assert_eq!(*order.lock().unwrap(), ["b", "c", "a"]);
    let fixes = run.run.document.fix.unwrap().module_fixes;
    assert_eq!(
        fixes
            .iter()
            .map(|row| row.module.as_str())
            .collect::<Vec<_>>(),
        ["b", "c", "a"]
    );
    let run = rig.run(true, &NoTerminal);
    assert!(run
        .run
        .document
        .fix
        .unwrap()
        .module_fixes
        .iter()
        .all(|row| row.skip_reason == Some(ModuleFixSkip::NoTerminal)));
}

#[test]
fn revert_or_overdue_manager_takes_precedence_over_module_fixes() {
    for overdue in [false, true] {
        let mut rig = Rig::new();
        rig.program("m", json!({"needs_stop": true}));
        rig.write_config();
        rig.offer_revert();
        let manager = Arc::new(Gate::default());
        if overdue {
            rig.host_mut().manager_gate = Some(manager.clone());
        } else {
            rig.host_mut().outcome = RevertOutcome::Reverted;
        }
        let run = rig.run(
            true,
            &Prompt(|_: &str| panic!("window revert takes priority")),
        );
        manager.open();
        let doc = serde_json::to_value(&run.run.document).unwrap();
        assert_eq!(
            doc["fix"]["module_fixes"],
            json!([{
                "module":"m", "finding":"module.failed", "class":"safe", "decision":"skipped",
                "skip_reason":if overdue { "manager_observation_overdue" } else { "revert_first" }, "result":null, "restart":null,
            }])
        );
        assert_eq!(
            rig.host.reverts.lock().unwrap().len(),
            usize::from(!overdue)
        );
        assert_eq!(
            doc["fix"]["skipped"],
            if overdue {
                json!("manager_observation_overdue")
            } else {
                Value::Null
            }
        );
    }
}

#[test]
fn first_identity_gate_precedes_even_the_post_consent_read() {
    for needs_stop in [false, true] {
        let mut rig = Rig::new();
        let path = rig.program("m", json!({"needs_stop": needs_stop, "record_fix": true}));
        rig.write_config();
        rig.script([Ok(vec![entry("m", true, "running")])], |_, _, _| {
            panic!("changed program must not be stopped")
        });
        let run = rig.run(
            true,
            &Prompt(|_: &str| {
                replace_file(&path);
                true
            }),
        );
        assert_eq!(
            first_fix(&run).skip_reason,
            Some(ModuleFixSkip::ProgramChanged)
        );
        assert!(!rig.marker("m").exists());
        assert_eq!(actions(&rig.host.daemon), [LIST]);
    }
}

#[test]
fn failed_signal_setup_checks_but_neither_prompts_nor_stops() {
    let mut rig = Rig::new();
    rig.program("m", json!({"needs_stop": true}));
    rig.write_config();
    rig.interrupt.signal_setup_failed();
    let run = rig.run(true, &Prompt(|_: &str| panic!("registration failed")));
    assert_eq!(run.checked.len(), 1);
    assert_eq!(
        first_fix(&run).skip_reason,
        Some(ModuleFixSkip::SignalSetupFailed)
    );
    assert_eq!(actions(&rig.host.daemon), [LIST]);
}

#[test]
fn owned_stop_waits_for_stopped_and_starts_even_on_fix_failure() {
    for (state, fix, outcome) in [
        ("running", "fixed", ModuleFixOutcome::Fixed),
        ("restarting", "failed", ModuleFixOutcome::Failed),
    ] {
        let mut rig = Rig::new();
        rig.program(
            "m",
            json!({"needs_stop": true, "fix": fix, "record_fix": true}),
        );
        let marker = rig.marker("m");
        rig.write_config();
        rig.script(
            [
                Ok(vec![entry("m", true, state)]),
                Ok(vec![entry("m", true, state)]),
                Ok(vec![entry("m", false, "draining")]),
                Ok(vec![entry("m", false, "stopped")]),
            ],
            move |_, enabled, _| {
                assert_eq!(
                    marker.exists(),
                    enabled,
                    "the fix runs between the stop and start"
                );
                Ok(true)
            },
        );
        let run = rig.run(true, &Prompt(|_: &str| true));
        let row = first_fix(&run);
        assert_eq!(result(row), outcome);
        assert_eq!(row.restart, Some(ModuleRestart::Enabled));
        assert_eq!(
            actions(&rig.host.daemon),
            [LIST, LIST, STOP, LIST, LIST, START, LIST]
        );
    }
}

#[test]
fn post_consent_disabled_absent_and_unapplied_stop_are_never_owned() {
    for (post_consent, expect_stop) in [
        (vec![entry("m", false, "stopped")], false),
        (vec![], false),
        (vec![entry("m", true, "running")], true),
    ] {
        let mut rig = Rig::new();
        rig.program("m", json!({"needs_stop": true, "fix":"module_running"}));
        rig.write_config();
        rig.script(
            [Ok(vec![entry("m", true, "running")]), Ok(post_consent)],
            move |_, enabled, _| {
                assert!(expect_stop);
                assert!(!enabled);
                Ok(false)
            },
        );
        let run = rig.run(true, &Prompt(|_: &str| true));
        assert_eq!(result(first_fix(&run)), ModuleFixOutcome::Refused);
        assert_eq!(first_fix(&run).restart, None);
        let expected: &[&str] = if expect_stop {
            &[LIST, LIST, STOP]
        } else {
            &[LIST, LIST]
        };
        assert_eq!(actions(&rig.host.daemon), expected);
    }
}

#[test]
fn error_reply_ownership_requires_evidence_and_operator_decline_never_owns() {
    for (code, after, owns, verify) in [
        ("stop_failed", entry("m", true, "running"), false, true),
        ("stop_failed", entry("m", false, "stopped"), true, true),
        ("stop_failed", entry("m", true, "draining"), true, true),
        (
            "operator_declined",
            entry("m", false, "stopped"),
            false,
            false,
        ),
        (
            "operator_presence_unavailable",
            entry("m", false, "stopped"),
            false,
            false,
        ),
    ] {
        let mut rig = Rig::new();
        rig.program("m", json!({"needs_stop":true, "record_fix":true}));
        rig.write_config();
        rig.script(
            [
                Ok(vec![]),
                Ok(vec![entry("m", true, "running")]),
                Ok(vec![after]),
            ],
            move |_, enabled, _| {
                if enabled {
                    Ok(false)
                } else {
                    Err(SetEnabledError::Reply(code.into()))
                }
            },
        );
        let run = rig.run(true, &Prompt(|_: &str| true));
        let row = first_fix(&run);
        assert_eq!(row.skip_reason, Some(ModuleFixSkip::StopFailed));
        assert_eq!(row.restart, owns.then_some(ModuleRestart::Enabled));
        assert!(!rig.marker("m").exists());
        let mut expected = vec![LIST, LIST, STOP];
        if verify {
            expected.push(LIST);
        }
        if owns {
            expected.push(START);
        }
        assert_eq!(actions(&rig.host.daemon), expected);
    }
}

#[test]
fn timed_out_stop_is_owned_without_consulting_an_old_snapshot() {
    let mut rig = Rig::new();
    rig.program("m", json!({"needs_stop":true, "record_fix":true}));
    rig.write_config();
    rig.script(
        [
            Ok(vec![]),
            Ok(vec![entry("m", true, "running")]),
            Ok(vec![entry("m", true, "running")]),
        ],
        |_, enabled, _| {
            if enabled {
                Ok(true)
            } else {
                Err(SetEnabledError::Rpc(RpcError::TimedOut))
            }
        },
    );
    let run = rig.run(true, &Prompt(|_: &str| true));
    assert_eq!(first_fix(&run).skip_reason, Some(ModuleFixSkip::StopFailed));
    assert_eq!(first_fix(&run).restart, Some(ModuleRestart::Enabled));
    assert_eq!(actions(&rig.host.daemon), [LIST, LIST, STOP, START]);
    assert!(!rig.marker("m").exists());
}

#[test]
fn dropped_stop_connection_is_replaced_for_restoration_and_later_reads() {
    for reconnects in [true, false] {
        let mut rig = Rig::new();
        rig.program("a", json!({"needs_stop":true, "record_fix":true}));
        rig.program("b", json!({"needs_stop":true, "fix":"module_running"}));
        rig.write_config();
        rig.script(
            [Ok(vec![]), Ok(vec![entry("a", true, "running")])],
            |_, enabled, _| {
                assert!(!enabled);
                Err(SetEnabledError::Rpc(RpcError::ConnectionLost))
            },
        );
        let replacement = Arc::new(daemon([Ok(vec![])], |module, enabled, _| {
            assert_eq!((module, enabled), ("a", true));
            Ok(false)
        }));
        rig.host
            .replacements
            .lock()
            .unwrap()
            .push_back(if reconnects {
                Ok(replacement.clone())
            } else {
                Err(RpcError::TimedOut)
            });
        let run = rig.run(true, &Prompt(|_: &str| true));
        assert_eq!(rig.host.connects.load(Ordering::Relaxed), 2);
        assert_eq!(actions(&rig.host.daemon), [LIST, LIST, STOP]);
        let row = first_fix(&run);
        assert_eq!(row.skip_reason, Some(ModuleFixSkip::StopFailed));
        if reconnects {
            assert_eq!(row.restart, Some(ModuleRestart::Enabled));
            assert_eq!(actions(&replacement), [START, LIST]);
        } else {
            assert_eq!(row.restart, Some(ModuleRestart::Failed));
            assert_eq!(run.run.exit_code, 1);
            assert_eq!(
                finding(&run.run, "module_program.restart_failed").evidence,
                json!({"error":"no_daemon"})
            );
        }
        assert_eq!(
            result(&run.run.document.fix.as_ref().unwrap().module_fixes[1]),
            ModuleFixOutcome::Refused
        );
    }
}

#[test]
fn replacement_during_stop_prevents_fix_but_restores_owned_stop() {
    let mut rig = Rig::new();
    let path = rig.program("m", json!({"needs_stop":true, "record_fix":true}));
    rig.write_config();
    rig.script(
        [
            Ok(vec![]),
            Ok(vec![entry("m", true, "running")]),
            Ok(vec![entry("m", false, "stopped")]),
        ],
        move |_, enabled, _| {
            if !enabled {
                replace_file(&path);
            }
            Ok(true)
        },
    );
    let run = rig.run(true, &Prompt(|_: &str| true));
    let row = first_fix(&run);
    assert_eq!(row.skip_reason, Some(ModuleFixSkip::ProgramChanged));
    assert_eq!(row.restart, Some(ModuleRestart::Enabled));
    assert!(!rig.marker("m").exists());
    assert_eq!(actions(&rig.host.daemon), [LIST, LIST, STOP, LIST, START]);
}

#[test]
fn start_failure_survives_clean_second_pass_once() {
    let mut rig = Rig::new();
    rig.program("m", json!({"needs_stop":true, "clean_after_fix":true}));
    rig.write_config();
    rig.script(
        [
            Ok(vec![]),
            Ok(vec![entry("m", true, "running")]),
            Ok(vec![entry("m", false, "stopped")]),
        ],
        |_, enabled, _| {
            if enabled {
                Err(SetEnabledError::Reply("start_failed".into()))
            } else {
                Ok(true)
            }
        },
    );
    let run = rig.run(true, &Prompt(|_: &str| true));
    assert_eq!(first_fix(&run).restart, Some(ModuleRestart::Failed));
    assert_eq!(run.run.exit_code, 1);
    assert!(run.run.document.ok);
    assert!(run.checked[0].check.findings().is_empty());
    assert_eq!(
        ids(&run.run)
            .iter()
            .filter(|id| **id == "module_program.restart_failed")
            .count(),
        1
    );
}

fn lost_start(retry_loses_connection: bool) -> ProgramRun {
    let mut rig = Rig::new();
    rig.program("m", json!({"needs_stop":true, "clean_after_fix":true}));
    rig.write_config();
    let first_deadline = Arc::new(Mutex::new(None));
    let recorded_deadline = first_deadline.clone();
    rig.script(
        [
            Ok(vec![]),
            Ok(vec![entry("m", true, "running")]),
            Ok(vec![entry("m", false, "stopped")]),
        ],
        move |id, enabled, deadline| {
            assert_eq!(id, "m");
            if enabled {
                *recorded_deadline.lock().unwrap() = Some(deadline);
                Err(SetEnabledError::Rpc(RpcError::ConnectionLost))
            } else {
                Ok(true)
            }
        },
    );
    let replacement = Arc::new(daemon([], move |id, enabled, deadline| {
        assert_eq!((id, enabled), ("m", true));
        assert_eq!(*first_deadline.lock().unwrap(), Some(deadline));
        assert!(Instant::now() < deadline);
        if retry_loses_connection {
            Err(SetEnabledError::Rpc(RpcError::ConnectionLost))
        } else {
            Ok(true)
        }
    }));
    rig.host
        .replacements
        .lock()
        .unwrap()
        .push_back(Ok(replacement.clone()));
    let run = rig.run(true, &Prompt(|_: &str| true));
    assert_eq!(
        actions(&rig.host.daemon),
        [LIST, LIST, STOP, LIST, START, LIST]
    );
    assert_eq!(actions(&replacement), [START]);
    assert_eq!(rig.host.connects.load(Ordering::Relaxed), 3);
    run
}

#[test]
fn lost_start_is_retried_with_the_same_deadline_on_the_replacement() {
    let run = lost_start(false);
    assert_eq!(first_fix(&run).restart, Some(ModuleRestart::Enabled));
    assert_eq!(run.run.exit_code, 0);
    assert!(!ids(&run.run).contains(&"module_program.restart_failed"));
}

#[test]
fn lost_start_retry_failure_warns_once_after_clean_second_pass() {
    let run = lost_start(true);
    assert_eq!(first_fix(&run).restart, Some(ModuleRestart::Failed));
    assert_eq!(run.run.exit_code, 1);
    assert!(run.run.document.ok);
    assert!(run.checked[0].check.findings().is_empty());
    let warnings = run
        .run
        .document
        .findings
        .iter()
        .filter(|row| row.id == "module_program.restart_failed")
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].severity, Severity::Warning);
    assert_eq!(warnings[0].owner, "m");
    assert_eq!(warnings[0].evidence, json!({"error":"connection_lost"}));
}

#[test]
fn interrupt_after_stop_decision_restores_only_owned_stops_and_skips_later_fixes() {
    for (kind, owns, verify) in [
        ("applied", true, false),
        ("unapplied", false, false),
        ("untouched", false, true),
        ("timed_out", true, false),
    ] {
        let mut rig = Rig::new();
        rig.program("a", json!({"needs_stop":true,"record_fix":true}));
        rig.program("b", json!({"record_fix":true}));
        rig.write_config();
        let interrupt = rig.interrupt.clone();
        rig.script(
            [
                Ok(vec![]),
                Ok(vec![entry("a", true, "running")]),
                Ok(vec![entry("a", true, "running")]),
            ],
            move |_, enabled, _| {
                if enabled {
                    return Ok(true);
                }
                assert!(interrupt.on_signal(15).is_some());
                match kind {
                    "applied" => Ok(true),
                    "unapplied" => Ok(false),
                    "untouched" => Err(SetEnabledError::Reply("stop_failed".into())),
                    _ => Err(SetEnabledError::Rpc(RpcError::TimedOut)),
                }
            },
        );
        let run = rig.run(
            true,
            &Prompt(|module: &str| {
                assert_eq!(module, "a");
                true
            }),
        );
        assert_eq!(rig.interrupt.signal(), Some(15));
        let rows = run.run.document.fix.unwrap().module_fixes;
        assert_eq!(
            rows[0].restart,
            owns.then_some(ModuleRestart::Enabled),
            "{kind}"
        );
        assert!(rows
            .iter()
            .all(|row| row.skip_reason == Some(ModuleFixSkip::Interrupted)));
        assert!(!rig.marker("a").exists());
        assert!(!rig.marker("b").exists());
        let mut expected = vec![LIST, LIST, STOP];
        if verify {
            expected.push(LIST);
        }
        if owns {
            expected.push(START);
        }
        assert_eq!(actions(&rig.host.daemon), expected);
    }
}

#[test]
fn signal_latched_during_first_pass_checks_prevents_window_revert() {
    let mut rig = Rig::new();
    rig.program("m", json!({"check_sleep_ms":300}));
    rig.write_config();
    rig.offer_revert();
    let run = thread::scope(|scope| {
        scope.spawn(|| {
            let start = Instant::now();
            loop {
                if let Some(wait) = rig.interrupt.on_signal(15) {
                    assert_eq!(wait.operation, "a module check");
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(2));
                thread::sleep(Duration::from_millis(5));
            }
        });
        rig.run(
            true,
            &Prompt(|_: &str| panic!("latched signal prevents prompting")),
        )
    });
    assert_eq!(rig.interrupt.signal(), Some(15));
    assert!(rig.host.reverts.lock().unwrap().is_empty());
    let fix = run.run.document.fix.unwrap();
    assert!(fix.result.is_none());
    assert_eq!(fix.skipped, Some(FixSkip::Interrupted));
    assert_eq!(
        fix.module_fixes[0].skip_reason,
        Some(ModuleFixSkip::Interrupted)
    );
}

#[test]
fn interrupt_guard_tracks_longest_remaining_work_and_latches_after_guards_drop() {
    let interrupt = Interrupt::default();
    assert!(interrupt.on_signal(2).is_none());
    let now = Instant::now();
    let short = interrupt
        .protect("short", now + Duration::from_secs(2))
        .unwrap();
    let long = interrupt
        .protect("long", now + Duration::from_secs(10))
        .unwrap();
    assert_eq!(interrupt.on_signal(15).unwrap().operation, "long");
    drop(long);
    assert_eq!(interrupt.on_signal(2).unwrap().operation, "short");
    drop(short);
    assert_eq!(interrupt.on_signal(2).unwrap().operation, "the report");
    assert_eq!(interrupt.signal(), Some(15));
    assert!(interrupt.protect("new", now).is_none());
}

#[test]
fn stop_and_start_have_independent_drain_and_operator_budgets() {
    let mut rig = Rig::new();
    rig.program("m", json!({"needs_stop":true,"fix":"sleep"}));
    rig.write_config();
    let budget = Duration::from_millis(2750);
    let stop_deadline = Mutex::new(None);
    rig.script(
        [
            Ok(vec![]),
            Ok(vec![entry("m", true, "running")]),
            Ok(vec![entry("m", false, "stopped")]),
        ],
        move |_, enabled, deadline| {
            let now = Instant::now();
            let remaining = deadline.duration_since(now);
            assert!(remaining <= budget && remaining > Duration::from_millis(2_500));
            if enabled {
                assert!(now > stop_deadline.lock().unwrap().unwrap());
            } else {
                *stop_deadline.lock().unwrap() = Some(deadline);
                // Longer than drain + margin, but within the operator budget.
                thread::sleep(Duration::from_millis(2_050));
                assert!(Instant::now() < deadline);
            }
            Ok(true)
        },
    );
    let run = rig.run(true, &Prompt(|_: &str| true));
    assert_eq!(result(first_fix(&run)), ModuleFixOutcome::Fixed);
    assert_eq!(first_fix(&run).restart, Some(ModuleRestart::Enabled));
}

#[test]
fn stop_accepts_a_drain_that_exceeds_the_ordinary_read_deadline() {
    let mut rig = Rig::new();
    rig.program("m", json!({"needs_stop":true}));
    rig.write_config();
    let running = entry("m", true, "running").with_drain_timeout_ms(Some(5_000));
    rig.script(
        [
            Ok(vec![]),
            Ok(vec![running]),
            Ok(vec![entry("m", false, "stopped")]),
        ],
        move |_, enabled, deadline| {
            if !enabled {
                thread::sleep(Duration::from_secs(5));
                assert!(Instant::now() < deadline);
            }
            Ok(true)
        },
    );
    let run = rig.run(true, &Prompt(|_: &str| true));
    assert_eq!(result(first_fix(&run)), ModuleFixOutcome::Fixed);
    assert_eq!(first_fix(&run).restart, Some(ModuleRestart::Enabled));
}

#[test]
fn shared_conformance_checks_the_fixture_output_and_read_only_check() {
    struct Adapter {
        program: PathBuf,
    }
    use cortexkit_doctor_conformance::{Adapter as Contract, Outcome};
    impl Contract for Adapter {
        fn program(&self) -> &Path {
            &self.program
        }
        fn seed(&mut self, data: &Path, _: &Path) -> std::io::Result<()> {
            let root = data.join("doctor-fixture");
            fs::create_dir(&root)?;
            fs::write(root.join("fixture.json"), b"{}")
        }
    }
    let mut rig = Rig::new();
    let program = rig.program("fixture", json!({}));
    let results = cortexkit_doctor_conformance::run(&mut Adapter { program });
    for expected in [
        "output.valid",
        "check.read_only",
        "manifest.valid",
        "manifest.digest",
    ] {
        assert_eq!(
            results
                .iter()
                .find(|row| row.name == expected)
                .unwrap()
                .outcome,
            Outcome::Passed,
            "{expected}"
        );
    }
}
