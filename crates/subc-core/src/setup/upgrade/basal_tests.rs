use super::*;
use crate::setup::{
    install_lock::{self, InstallLock},
    model::{PlanOutcome, UpgradeOperation},
    planner::{plan_upgrade, UpgradePlan},
    runtime::{CommandResult, CommandRunner},
    update_cache::CachedRelease,
    upgrade_executor::execute_upgrade,
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};

const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

fn targets() -> [UpgradeTarget; 2] {
    // Keep the test independent of the upgrade roster so a missing Basal row
    // cannot make the lifecycle tests pass with an empty plan.
    [
        UpgradeTarget {
            component: Component::Basal,
            binary: "ck-basal",
            module_id: Some("basal"),
        },
        UpgradeTarget {
            component: Component::Basal,
            binary: "ck-basal-worker",
            module_id: None,
        },
    ]
}

#[test]
fn basal_component_table_pins_programs_and_needs() {
    assert_eq!(Component::Basal.plain_name(), Some("Automations"));
    assert_eq!(
        components::component_needs(Component::Basal),
        [Component::Core, Component::Cingulate, Component::Entorhinal]
    );
    assert_eq!(
        components::needs_closure([Component::Basal]),
        BTreeSet::from([
            Component::Core,
            Component::Claustrum,
            Component::Cingulate,
            Component::Entorhinal,
            Component::Basal
        ])
    );
    for platform in AlphaTarget::ALL {
        assert_eq!(
            components::component_binaries_for_target(Component::Basal, platform),
            ["ck-basal", "ck-basal-worker"]
        );
    }
    assert_eq!(upgrade_roster([Component::Basal]), targets());
}

fn observed(worker_available: bool) -> UpgradeObserved {
    let targets = targets();
    let metadata = UpdateMetadata {
        format_version: 1,
        checked_at_unix_secs: 1,
        targets: targets
            .into_iter()
            .enumerate()
            .map(|(i, target)| {
                (
                    target.label().into(),
                    CachedRelease {
                        version: "0.2.0".into(),
                        sha256: (i == 0 || worker_available)
                            .then(|| if i == 0 { "aa" } else { "bb" }.repeat(32)),
                        reports_release_version: i == 0,
                    },
                )
            })
            .collect(),
    };
    let discovered = targets
        .into_iter()
        .enumerate()
        .map(|(i, target)| ManagedUpgradeTarget {
            target,
            destination: PathBuf::from(target.label()),
            // The worker's semver is independent of Basal's release version.
            installed_version: if i == 0 { "0.2.0" } else { "0.7.0" }.into(),
            installed_archive_sha256: Some("aa".repeat(32)),
            registration_channel_missing: false,
        })
        .collect::<Vec<_>>();
    observed_upgrade_targets(
        &metadata,
        &discovered,
        Ok(BTreeSet::from(["basal".into()])),
        None,
    )
}

#[test]
fn basal_worker_only_update_places_both_and_requires_both_assets() {
    use UpgradeOperation::*;
    let [module, worker] = targets();
    let plan = plan_upgrade(&observed(true));
    assert_eq!(
        plan.operations,
        [
            ObservePlatform,
            DownloadAndVerify { target: worker },
            DownloadAndVerify { target: module },
            CreateRollbackCopy { target: worker },
            CreateRollbackCopy { target: module },
            StopModule { target: module },
            ReplaceDestination { target: worker },
            ReplaceDestination { target: module },
            WarmExecute { target: worker },
            WarmExecute { target: module },
            StartModule { target: module },
            PollModuleRestartCompletion { target: module },
            PostVerify { target: worker },
            PostVerify { target: module },
        ]
    );
    assert_eq!(
        plan.outcomes
            .iter()
            .filter(|outcome| matches!(outcome, PlanOutcome::UpgradeAvailable { .. }))
            .count(),
        2
    );
    let incomplete = plan_upgrade(&observed(false));
    assert_eq!(incomplete.operations, [ObservePlatform]);
    assert!(incomplete.outcomes.iter().any(|outcome| matches!(outcome, PlanOutcome::ReleaseIncomplete { component: Component::Basal, missing_asset, .. } if missing_asset.starts_with("ck-basal-worker-"))));
}

fn script(target: UpgradeTarget, upgraded: bool) -> Vec<u8> {
    let version = match (target.module_id().is_some(), upgraded) {
        (true, true) => "0.2.0",
        (true, false) => "0.1.0",
        (false, true) => "0.7.1",
        (false, false) => "0.7.0",
    };
    let revision = if upgraded { REVISION } else { "old" };
    format!(
        "#!/bin/sh\necho '{} {version} ({revision})'\n",
        target.label()
    )
    .into_bytes()
}

#[derive(Default)]
struct Process {
    enabled: bool,
    live: bool,
    pid: u64,
    remaining_stop_polls: Option<usize>,
    calls: Vec<&'static str>,
}

#[derive(Clone)]
struct Runner {
    bin: PathBuf,
    process: Arc<Mutex<Process>>,
    fail_placement: Option<PathBuf>,
    never_stop: bool,
    expect_new_on_start: bool,
}

impl Runner {
    fn assert_pair(&self, upgraded: bool) {
        for target in targets() {
            assert_eq!(
                fs::read(self.bin.join(target.label())).unwrap(),
                script(target, upgraded)
            );
        }
    }
}

impl CommandRunner for Runner {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        let mut state = self.process.lock().unwrap();
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let stdout = match args.as_slice() {
            ["--json", "module", "list"] => {
                serde_json::json!({"modules": [{"module_id": "basal", "enabled": state.enabled}]})
                    .to_string()
            }
            ["--json", "provenance", "basal"] => {
                if let Some(remaining) = state.remaining_stop_polls.as_mut() {
                    if *remaining == 0 {
                        state.live = false;
                        state.remaining_stop_polls = None;
                        state.calls.push("stopped");
                    } else {
                        *remaining -= 1;
                        state.calls.push("wait");
                    }
                }
                let observed = if state.live {
                    serde_json::json!({"pid": state.pid, "spawned_at_ms": state.pid, "running_image": {"status": "match"}})
                } else {
                    serde_json::json!({"running_image": {"reason": "not_running"}})
                };
                serde_json::json!({"modules": [{"module_id": "basal", "daemon_observed": observed}]}).to_string()
            }
            ["module", "stop", "basal"] => {
                if state.enabled {
                    self.assert_pair(false);
                    state.enabled = false;
                    state.remaining_stop_polls = (!self.never_stop).then_some(1);
                    state.calls.push("stop");
                    if let Some(candidate) = self.fail_placement.take() {
                        fs::remove_file(candidate).unwrap();
                    }
                }
                "stop acknowledged".into()
            }
            ["module", "start", "basal"] => {
                self.assert_pair(self.expect_new_on_start);
                state.enabled = true;
                if !state.live {
                    state.pid += 1;
                    state.live = true;
                }
                state.calls.push("start");
                "start acknowledged".into()
            }
            ["--json", "module", "status", "basal"] => {
                state.calls.push("health");
                serde_json::json!({"module": {"live": state.live}, "health": {"status": "healthy"}})
                    .to_string()
            }
            ["--user", "is-active", ..] if program == "systemctl" => "active".into(),
            ["--user", "is-enabled", ..] if program == "systemctl" => "enabled".into(),
            _ => panic!("unexpected command {program} {args:?}"),
        };
        Ok(CommandResult {
            success: true,
            stdout,
            ..Default::default()
        })
    }
}

struct Fixture {
    _root: cortexkit_test_support::ScratchDir,
    backend: SystemUpgradeBackend,
    runner: Runner,
}

impl Fixture {
    fn new(running: bool, fail_placement: bool, never_stop: bool) -> Self {
        let root = cortexkit_test_support::ScratchDir::new("basal-upgrade");
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let [module, worker] = targets();
        let mut backend = tests::managed_backend(root.path(), module, &bin.join("ck-basal"));
        backend.targets.insert(
            worker.label().into(),
            ManagedUpgradeTarget {
                target: worker,
                destination: bin.join(worker.label()),
                installed_version: "0.7.0".into(),
                installed_archive_sha256: None,
                registration_channel_missing: false,
            },
        );
        backend.inventory.record(
            "managed-binary",
            &bin.join(worker.label()),
            serde_json::Map::new(),
        );
        backend.inventory.save().unwrap();
        backend.set_expected_version(worker, "0.2.0".into());
        backend.assets = ReleaseUpgradeAssetFetcher::from_index(
            serde_json::from_value(serde_json::json!({
                "schema": 1, "channel": "alpha", "generated_at_ms": 0,
                "components": {"basal": {"release": "v0.2.0", "version": "0.2.0", "assets": {
                "linux-x64": {"ck-basal": asset(Some(REVISION)), "ck-basal-worker": asset(None)}
                }}}
            }))
            .unwrap(),
        );
        for target in targets() {
            let destination = bin.join(target.label());
            fs::write(&destination, script(target, false)).unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
            let workspace = root.join(format!("candidate-{}", target.label()));
            fs::create_dir(&workspace).unwrap();
            let candidate = workspace.join(target.label());
            fs::write(&candidate, script(target, true)).unwrap();
            fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
            backend.prepared.insert(
                target.label().into(),
                PreparedUpgradeAsset::test_candidate(candidate, target),
            );
        }
        let runner = Runner {
            bin,
            process: Arc::new(Mutex::new(Process {
                enabled: running,
                live: running,
                pid: 1,
                ..Default::default()
            })),
            fail_placement: fail_placement
                .then(|| backend.prepared[module.label()].candidate.clone()),
            never_stop,
            expect_new_on_start: !fail_placement && !never_stop,
        };
        backend.runner = Box::new(runner.clone());
        backend.inventory = Inventory::load_locked(
            backend.inventory.manifest(),
            "linux-x64",
            "basal test",
            install_lock::WAIT,
        )
        .unwrap();
        Self {
            _root: root,
            backend,
            runner,
        }
    }
}

fn asset(reports: Option<&str>) -> Value {
    serde_json::json!({"url": "https://example.invalid/release", "sha256": "aa".repeat(32), "bytes": 10, "reports": reports})
}

fn prepared_plan() -> UpgradePlan {
    let mut plan = plan_upgrade(&observed(true));
    // Network downloads are covered by the signed setup fixture. Here the
    // candidates already represent the verified bytes handed to the upgrade sequence.
    plan.operations
        .retain(|op| !matches!(op, UpgradeOperation::DownloadAndVerify { .. }));
    plan
}

#[test]
fn basal_running_upgrade_stops_and_waits_then_places_worker_before_module() {
    let mut fixture = Fixture::new(true, false, false);
    let report = execute_upgrade(&prepared_plan(), &mut fixture.backend).unwrap();
    let steps = report
        .evidence
        .iter()
        .map(|entry| (entry.stage, entry.target.label()))
        .collect::<Vec<_>>();
    assert!(steps.windows(4).any(|steps| steps
        == [
            ("module-stop", "ck-basal"),
            ("destination-replacement", "ck-basal-worker"),
            ("destination-replacement", "ck-basal"),
            ("warm-execution", "ck-basal-worker"),
        ]));
    assert!(report
        .evidence
        .iter()
        .any(|entry| entry.stage == "post-verification"
            && entry.target.label() == "ck-basal-worker"
            && entry.detail.contains("0.7.1")));
    assert_eq!(
        fixture.runner.process.lock().unwrap().calls,
        ["stop", "wait", "stopped", "start", "health", "health"]
    );
    fixture.runner.assert_pair(true);
}

#[test]
fn basal_stopped_upgrade_places_pair_without_stop_start_or_poll() {
    let mut fixture = Fixture::new(false, false, false);
    execute_upgrade(&prepared_plan(), &mut fixture.backend).unwrap();
    fixture.runner.assert_pair(true);
    let process = fixture.runner.process.lock().unwrap();
    assert!(!process.enabled && !process.live && process.calls.is_empty());
}

#[test]
fn basal_second_placement_failure_restores_old_pair_before_start() {
    let mut fixture = Fixture::new(true, true, false);
    let error = execute_upgrade(&prepared_plan(), &mut fixture.backend).unwrap_err();
    assert!(
        !error.reason.contains("install recovery stopped"),
        "{error}"
    );
    assert_eq!(error.stage, "destination-replacement");
    assert_eq!(error.target.label(), "ck-basal");
    assert!(error
        .report
        .evidence
        .iter()
        .any(|entry| entry.stage == "destination-replacement"
            && entry.target.label() == "ck-basal-worker"));
    assert_eq!(
        fixture.runner.process.lock().unwrap().calls,
        ["stop", "wait", "stopped", "start"]
    );
    fixture.runner.assert_pair(false);
}

#[test]
fn basal_stop_timeout_does_not_place_binaries_and_restores_enabled_state() {
    let mut fixture = Fixture::new(true, false, true);
    let [module, worker] = targets();
    for target in [worker, module] {
        fixture.backend.create_rollback_copy(target).unwrap();
    }
    let error = fixture
        .backend
        .stop_module(module, Duration::ZERO)
        .unwrap_err();
    assert!(error.contains("did not stop"), "{error}");
    assert!(fixture.backend.abort_before_activation().unwrap());
    fixture.runner.assert_pair(false);
    let process = fixture.runner.process.lock().unwrap();
    assert_eq!(process.calls, ["stop", "start"]);
    assert!(process.live && process.enabled);
    assert_eq!(process.pid, 1);
}

#[test]
fn basal_interrupted_cut_recovers_old_pair_then_starts_it() {
    let mut fixture = Fixture::new(true, false, false);
    let [module, worker] = targets();
    for target in [worker, module] {
        fixture.backend.create_rollback_copy(target).unwrap();
    }
    fixture
        .backend
        .stop_module(module, Duration::from_secs(1))
        .unwrap();
    fixture.backend.replace_destination(worker).unwrap();
    assert_eq!(
        fs::read(fixture.runner.bin.join(worker.label())).unwrap(),
        script(worker, true)
    );
    let paths = fixture.backend.runtime_paths().unwrap();
    let manifest = fixture.backend.inventory.manifest().to_owned();
    let ck = fixture.backend.executable.clone();
    drop(fixture.backend);
    let _lock =
        InstallLock::acquire_within(&manifest, "basal test recovery", install_lock::WAIT).unwrap();
    fixture.runner.expect_new_on_start = false;
    crate::setup::install_journal::system::recover_locked(
        &manifest,
        &ck,
        || None,
        &paths,
        &mut fixture.runner,
    )
    .unwrap();
    fixture.runner.assert_pair(false);
    assert_eq!(
        fixture.runner.process.lock().unwrap().calls,
        ["stop", "wait", "stopped", "start"]
    );
}
