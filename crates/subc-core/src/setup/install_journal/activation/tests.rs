use super::super::{bootstrap::FileRecovery, files, Outcome, Recovery};
use super::*;
use std::path::PathBuf;

fn live(identity: &str, build: &Build) -> Live {
    Live {
        identity: identity.into(),
        build: build.clone(),
    }
}

struct Runtime {
    live: Option<Live>,
    installed: Build,
    calls: Vec<&'static str>,
    fail_after_stop: bool,
}

impl Control for Runtime {
    fn live(&mut self, _: Option<&str>) -> Result<Option<Live>, String> {
        Ok(self.live.clone())
    }

    fn stop(&mut self, _: Option<&str>) -> Result<(), String> {
        self.calls.push("stop");
        self.live = None;
        if std::mem::take(&mut self.fail_after_stop) {
            return Err("interrupted after quiesce".into());
        }
        Ok(())
    }

    fn start(&mut self, _: Option<&str>) -> Result<(), String> {
        self.calls.push("start");
        self.live = Some(live("new process", &self.installed));
        Ok(())
    }

    fn restart(&mut self, module: Option<&str>) -> Result<(), String> {
        self.stop(module)?;
        self.start(module)
    }
}

impl Recovery for Runtime {
    fn is_applied(&mut self, step: &Step) -> Result<bool, String> {
        if step.action.file().is_some() {
            FileRecovery.is_applied(step)
        } else {
            verify(step, self)
        }
    }
    fn can_replay(&mut self, id: &str, step: &Step) -> Result<bool, String> {
        if step.action.file().is_some() {
            FileRecovery.can_replay(id, step)
        } else {
            can_replay(step, self)
        }
    }
    fn replay(&mut self, id: &str, step: &Step) -> Result<(), String> {
        if step.action.file().is_some() {
            FileRecovery.replay(id, step)
        } else {
            replay(step, self)
        }
    }
    fn undo(&mut self, id: &str, phase: Undo, step: &Step) -> Result<(), String> {
        if step.action.file().is_some() {
            FileRecovery.undo(id, phase, step)
        } else {
            undo(step, phase, self)
        }
    }
    fn prepare_discard(
        &mut self,
        id: &str,
        step: &Step,
        outcome: Outcome,
    ) -> Result<Option<String>, String> {
        if step.action.file().is_some() {
            FileRecovery.prepare_discard(id, step, outcome)
        } else {
            Ok(None)
        }
    }
    fn discard(
        &mut self,
        id: &str,
        step: &Step,
        path: &Path,
        identity: &str,
    ) -> Result<Vec<PathBuf>, crate::setup::install_journal::CleanupRefusal> {
        FileRecovery.discard(id, step, path, identity)
    }
}

fn build(tag: &str) -> Build {
    Build::Image { sha256: tag.into() }
}

#[test]
fn undo_reenables_a_supervised_module_that_had_no_pid_without_waiting_for_a_build() {
    struct Backoff {
        enabled: bool,
        starts: usize,
    }
    impl Control for Backoff {
        fn live(&mut self, _: Option<&str>) -> Result<Option<Live>, String> {
            Ok(None)
        }
        fn stop(&mut self, _: Option<&str>) -> Result<(), String> {
            self.enabled = false;
            Ok(())
        }
        fn start(&mut self, _: Option<&str>) -> Result<(), String> {
            self.enabled = true;
            self.starts += 1;
            Ok(())
        }
        fn restart(&mut self, _: Option<&str>) -> Result<(), String> {
            unreachable!()
        }
        fn intent(&mut self, _: Option<&str>) -> Result<Option<Intent>, String> {
            Ok(Some(Intent::Module {
                enabled: self.enabled,
            }))
        }
    }
    let root = cortexkit_test_support::ScratchDir::new("activation-enabled-backoff");
    let manifest = root.join("manifest");
    let mut journal = Journal::create(&manifest).unwrap();
    let mut control = Backoff {
        enabled: true,
        starts: 0,
    };
    let index = prepare(
        &mut journal,
        &root.join("module"),
        Some("backoff"),
        Some(build("new")),
        &mut control,
    )
    .unwrap();
    drop(journal);
    let journal = Journal::open(&manifest).unwrap().unwrap();
    let step = journal.step(index);
    undo(step, Undo::Quiesce, &mut control).unwrap();
    assert!(!control.enabled);
    undo(step, Undo::Reactivate, &mut control).unwrap();
    assert!(control.enabled, "the enabled intent must survive rollback");
    assert_eq!(control.starts, 1);
    assert!(control.live(Some("backoff")).unwrap().is_none());
}

#[test]
fn undo_restores_a_stopped_daemons_registration_without_starting_it() {
    struct Stopped {
        registered: bool,
        restores: Vec<bool>,
    }
    impl Control for Stopped {
        fn live(&mut self, _: Option<&str>) -> Result<Option<Live>, String> {
            Ok(None)
        }
        fn stop(&mut self, _: Option<&str>) -> Result<(), String> {
            self.registered = false;
            Ok(())
        }
        fn start(&mut self, _: Option<&str>) -> Result<(), String> {
            panic!("undo must not start a daemon that was stopped")
        }
        fn restart(&mut self, _: Option<&str>) -> Result<(), String> {
            unreachable!()
        }
        fn intent(&mut self, _: Option<&str>) -> Result<Option<Intent>, String> {
            Ok(Some(Intent::Daemon {
                registered: self.registered,
            }))
        }
        fn restore_daemon_registration(&mut self, registered: bool) -> Result<(), String> {
            self.restores.push(registered);
            self.registered = registered;
            Ok(())
        }
    }
    for registered in [false, true] {
        let root = cortexkit_test_support::ScratchDir::new("activation-stopped-registration");
        let manifest = root.join("manifest");
        let mut journal = Journal::create(&manifest).unwrap();
        let mut control = Stopped {
            registered,
            restores: vec![],
        };
        let index = prepare(
            &mut journal,
            &root.join("daemon"),
            None,
            Some(build("new")),
            &mut control,
        )
        .unwrap();
        let step = journal.step(index);
        undo(step, Undo::Quiesce, &mut control).unwrap();
        assert!(!control.registered);
        undo(step, Undo::Reactivate, &mut control).unwrap();
        assert_eq!(control.restores, [registered]);
        assert_eq!(control.registered, registered);
    }
}

#[test]
fn undo_cancels_a_queued_candidate_before_restoring_files() {
    struct Starting {
        polls: usize,
        cancelled: bool,
        live: Option<Live>,
    }
    impl Control for Starting {
        fn live(&mut self, _: Option<&str>) -> Result<Option<Live>, String> {
            self.polls += 1;
            if self.polls >= 3 && !self.cancelled {
                self.live = Some(live("queued candidate", &build("new")));
            }
            Ok(self.live.clone())
        }
        fn stop(&mut self, _: Option<&str>) -> Result<(), String> {
            self.cancelled = true;
            self.live = None;
            Ok(())
        }
        fn start(&mut self, _: Option<&str>) -> Result<(), String> {
            self.live = Some(live("previous release restarted", &build("old")));
            Ok(())
        }
        fn restart(&mut self, _: Option<&str>) -> Result<(), String> {
            unreachable!()
        }
    }
    for module in [None, Some("fixture-module".to_string())] {
        let step = Step {
            target: PathBuf::from("fixture-daemon"),
            action: Action::Activation {
                module,
                before: Some(live("original", &build("old"))),
                after: Some(build("new")),
                before_intent: None,
            },
        };
        let mut control = Starting {
            polls: 0,
            cancelled: false,
            live: None,
        };
        undo(&step, Undo::Quiesce, &mut control).unwrap();
        undo(&step, Undo::Reactivate, &mut control).unwrap();
        assert!(
            control.cancelled,
            "quiesce must cancel the queued start even if no PID is visible"
        );
        assert_eq!(control.live.unwrap().build, build("old"));
    }
}

#[test]
fn reactivation_mismatch_names_the_module_and_its_stop_command() {
    let mut runtime = Runtime {
        live: Some(live("unexpected", &build("wrong"))),
        installed: build("previous"),
        calls: vec![],
        fail_after_stop: false,
    };
    let step = Step {
        target: PathBuf::from("module-program"),
        action: Action::Activation {
            module: Some("fixture-module".into()),
            before: Some(live("old", &build("previous"))),
            after: Some(build("next")),
            before_intent: None,
        },
    };
    let error = undo(&step, Undo::Reactivate, &mut runtime).unwrap_err();
    assert!(error.contains("ck module stop fixture-module"), "{error}");
    assert!(error.contains("rerun the installer command"), "{error}");
}

#[test]
fn an_unconfirmed_sealed_activation_commits_only_after_the_new_build_is_live() {
    for already_running in [false, true] {
        let scratch = cortexkit_test_support::ScratchDir::new("journal-late-activation");
        let root = std::fs::canonicalize(scratch.path()).unwrap();
        let manifest = root.join("installer-manifest.json");
        let old = build("old image");
        let new = build("new image");
        let mut runtime = Runtime {
            live: Some(live("old", &old)),
            installed: new.clone(),
            calls: vec![],
            fail_after_stop: false,
        };
        let mut journal = Journal::create(&manifest).unwrap();
        prepare(
            &mut journal,
            &root.join("daemon"),
            None,
            Some(new.clone()),
            &mut runtime,
        )
        .unwrap();
        if already_running {
            runtime.live = Some(live("new", &new));
        }
        files::prepare(
            &mut journal,
            &manifest,
            files::Kind::Inventory,
            &files::Content::Bytes(b"new ownership", 0o600),
        )
        .unwrap();
        drop(journal);
        assert!(!manifest.exists());
        let outcome = Journal::open(&manifest)
            .unwrap()
            .unwrap()
            .recover(&mut runtime)
            .unwrap();
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(std::fs::read(manifest).unwrap(), b"new ownership");
        assert_eq!(runtime.live.unwrap().build, new);
        assert_eq!(
            runtime.calls,
            if already_running {
                vec![]
            } else {
                vec!["stop", "start"]
            }
        );
    }
}

#[test]
fn restart_needs_a_new_identity_even_when_the_expected_build_is_unchanged() {
    let scratch = cortexkit_test_support::ScratchDir::new("journal-restart-identity");
    let manifest = scratch.path().join("manifest");
    let image = build("matching-build");
    let mut runtime = Runtime {
        live: Some(live("original", &image)),
        installed: image.clone(),
        calls: vec![],
        fail_after_stop: false,
    };
    let mut journal = Journal::create(&manifest).unwrap();
    let index = prepare(
        &mut journal,
        &PathBuf::from("service"),
        None,
        Some(image),
        &mut runtime,
    )
    .unwrap();
    let step = journal.step(index);
    assert!(!verify(step, &mut runtime).unwrap());
    replay(step, &mut runtime).unwrap();
    assert!(verify(step, &mut runtime).unwrap());
    assert_eq!(runtime.calls, ["stop", "start"]);
}

#[test]
fn interrupted_activation_undo_resumes_after_stopping_the_candidate() {
    let scratch = cortexkit_test_support::ScratchDir::new("journal-restart-recovery");
    let manifest = scratch.path().join("manifest");
    let old = build("old");
    let new = build("new");
    let mut runtime = Runtime {
        live: Some(live("original", &old)),
        installed: new.clone(),
        calls: vec![],
        fail_after_stop: false,
    };
    let mut journal = Journal::create(&manifest).unwrap();
    let index = prepare(
        &mut journal,
        &PathBuf::from("module"),
        Some("example"),
        Some(new),
        &mut runtime,
    )
    .unwrap();
    replay(journal.step(index), &mut runtime).unwrap();
    runtime.calls.clear();
    runtime.fail_after_stop = true;
    assert_eq!(
        journal.recover(&mut runtime).unwrap_err(),
        "interrupted after quiesce"
    );
    assert!(runtime.live.is_none());
    runtime.installed = old.clone();
    let journal = Journal::open(&manifest).unwrap().unwrap();
    assert_eq!(journal.recover(&mut runtime).unwrap(), Outcome::Undone);
    assert_eq!(runtime.live.unwrap().build, old);
    assert_eq!(runtime.calls, ["stop", "stop", "start"]);
}

#[test]
fn authenticated_image_is_used_only_when_the_release_has_no_expected_declaration() {
    let response = serde_json::json!({
        "module_declared": {
            "build": { "build_git_sha": "a".repeat(40), "build_lock_digest": "b".repeat(64) }
        },
        "daemon_observed": { "running_image": { "status": "match" } }
    });
    let expected = Build::Declared {
        git_sha: "c".repeat(40),
        lock_digest: "b".repeat(64),
    };
    let error = module_build(Some(&expected), &response, Some("digest".into())).unwrap_err();
    assert!(error.contains("differs"), "{error}");
    assert_eq!(
        module_build(None, &response, Some("digest".into())).unwrap(),
        build("digest")
    );
    let declared = daemon_build(&response["module_declared"]["build"]).unwrap();
    assert_eq!(
        module_build(Some(&declared), &response, None).unwrap(),
        declared
    );
}
