use super::*;
use crate::setup::install_journal::{Outcome, Recovery};
#[cfg(unix)]
use serde_json::{Map, Value};
use std::fs;
use std::path::PathBuf;

#[derive(Default)]
struct Manager {
    loaded: PathBuf,
    probes: Vec<PathBuf>,
    enabled: bool,
    stale: bool,
}

impl CommandRunner for Manager {
    fn run(&mut self, program: &str, args: &[String]) -> Result<runtime::CommandResult, String> {
        if program == "schtasks.exe" {
            return Ok(runtime::CommandResult {
                success: self.enabled,
                exit_code: Some(if self.enabled { 0 } else { 1 }),
                ..runtime::CommandResult::default()
            });
        }
        assert_eq!(program, "systemctl");
        let result = match args[1].as_str() {
            "is-enabled" => (self.enabled, String::new()),
            "is-active" => (false, String::new()),
            "show" => (
                true,
                format!(
                    "FragmentPath={}\nNeedDaemonReload={}",
                    self.loaded.display(),
                    if self.stale { "yes" } else { "no" }
                ),
            ),
            "daemon-reload" => {
                self.loaded = self
                    .probes
                    .iter()
                    .find(|path| path.is_file())
                    .cloned()
                    .unwrap_or_default();
                self.stale = false;
                (true, String::new())
            }
            "enable" => {
                self.enabled = true;
                (true, String::new())
            }
            "disable" => {
                self.enabled = false;
                (true, String::new())
            }
            command => panic!("unexpected systemctl command {command}"),
        };
        Ok(runtime::CommandResult {
            success: result.0,
            exit_code: Some(if result.0 { 0 } else { 1 }),
            stdout: result.1,
            stderr: String::new(),
        })
    }
}

impl Recovery for Service<'_, Manager> {
    fn is_applied(&mut self, step: &Step) -> Result<bool, String> {
        self.is_applied_with_mark(step, false)
    }
    fn is_applied_with_mark(&mut self, step: &Step, done: bool) -> Result<bool, String> {
        match step.action {
            Action::Registration { .. } => self.verify(step, done),
            #[cfg(unix)]
            Action::Symlink(_) => super::super::symlink::verify(step),
            _ => files::verify(step),
        }
    }
    fn can_replay(&mut self, id: &str, step: &Step) -> Result<bool, String> {
        match step.action {
            Action::Registration { .. } => self.can_replay(step),
            #[cfg(unix)]
            Action::Symlink(_) => super::super::symlink::can_replay(id, step),
            _ => files::can_replay(id, step),
        }
    }
    fn replay(&mut self, id: &str, step: &Step) -> Result<(), String> {
        match step.action {
            Action::Registration { .. } => self.replay(step),
            #[cfg(unix)]
            Action::Symlink(_) => super::super::symlink::replay(id, step),
            _ => files::replay(id, step),
        }
    }
    fn undo(&mut self, id: &str, phase: Undo, step: &Step) -> Result<(), String> {
        match (&step.action, phase) {
            (Action::Registration { .. }, _) => self.undo(step, phase),
            #[cfg(unix)]
            (Action::Symlink(_), Undo::Restore) => super::super::symlink::restore(id, step),
            (_, Undo::Restore) => files::restore(id, step),
            _ => Ok(()),
        }
    }
    fn prepare_discard(
        &mut self,
        id: &str,
        step: &Step,
        _: Outcome,
    ) -> Result<Option<String>, String> {
        if step.action.file().is_some() {
            files::prepare_discard(id, step)
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
        files::discard(id, step, path, identity)
    }
}

const OLD: &str = "[Service]\nExecStart=/old\n[Install]\nWantedBy=default.target\n";

#[test]
fn a_stopped_registered_launch_agent_is_refused_with_both_safe_choices() {
    let stopped = runtime::RuntimeStatus {
        registered: true,
        live: false,
    };
    let error = refuse_stopped_launch_agent(RuntimePlatform::Macos, stopped).unwrap_err();
    let job = format!("gui/{}/cortexkit.subc", runtime::current_uid());
    assert!(
        error.contains(r#"launchctl kickstart "gui/$(id -u)/cortexkit.subc""#),
        "{error}"
    );
    assert!(
        error.contains(&format!("launchctl bootout {job}")),
        "{error}"
    );
    assert!(refuse_stopped_launch_agent(RuntimePlatform::Linux, stopped).is_ok());
    assert!(
        refuse_stopped_launch_agent(RuntimePlatform::Macos, runtime::RuntimeStatus::default())
            .is_ok()
    );
}
#[cfg(unix)]
const NEW: &str = "[Service]\nExecStart=/new\n[Install]\nWantedBy=default.target\n";

fn paths(root: &Path) -> RuntimePaths {
    RuntimePaths {
        definition: root.join("new/cortexkit-subc.service"),
        daemon: root.join("ck-subc"),
        config_store_root: root.join("data"),
    }
}

#[test]
fn a_loaded_unit_needing_reload_does_not_verify_the_registration() {
    let fixture = cortexkit_test_support::ScratchDir::new("journal-fragment");
    let paths = paths(fixture.path());
    let mut runner = Manager {
        loaded: paths.definition.clone(),
        enabled: true,
        stale: true,
        ..Manager::default()
    };
    let mut service = Service {
        platform: RuntimePlatform::Linux,
        paths: &paths,
        runner: &mut runner,
    };
    let facts = service.record(&paths.definition, true, None);
    assert!(!service.matches(&facts).unwrap());
    service.runner.stale = false;
    assert!(service.matches(&facts).unwrap());
    service.runner.loaded = paths.definition.with_extension("other");
    assert!(!service.matches(&facts).unwrap());
}

#[test]
fn a_windows_same_name_registration_needs_a_done_mark() {
    let fixture = cortexkit_test_support::ScratchDir::new("journal-task-registration");
    let paths = paths(fixture.path());
    let mut runner = Manager {
        enabled: true,
        ..Manager::default()
    };
    let mut service = Service {
        platform: RuntimePlatform::Windows,
        paths: &paths,
        runner: &mut runner,
    };
    let before = service.record(&paths.definition, true, Some("before".into()));
    let after = service.record(&paths.definition, true, Some("after".into()));
    let mut journal = Journal::create(&fixture.path().join("manifest")).unwrap();
    let index = service.prepare(&mut journal, before, after).unwrap();
    assert!(!service.verify(journal.step(index), false).unwrap());
    assert!(!service.can_replay(journal.step(index)).unwrap());
    assert!(service.verify(journal.step(index), true).unwrap());
}

#[test]
fn undo_after_deregistration_reenables_the_recorded_unit() {
    let fixture = cortexkit_test_support::ScratchDir::new("journal-deregister");
    let paths = paths(fixture.path());
    fs::create_dir_all(paths.definition.parent().unwrap()).unwrap();
    fs::write(&paths.definition, OLD).unwrap();
    let mut runner = Manager {
        loaded: paths.definition.clone(),
        probes: vec![paths.definition.clone()],
        enabled: true,
        ..Manager::default()
    };
    let mut service = Service {
        platform: RuntimePlatform::Linux,
        paths: &paths,
        runner: &mut runner,
    };
    let manifest = fixture.path().join("manifest");
    let mut journal = Journal::create(&manifest).unwrap();
    let before = service.record(
        &paths.definition,
        true,
        files::digest(&paths.definition).unwrap(),
    );
    let after = service.record(&paths.definition, false, None);
    let index = service.prepare(&mut journal, before, after).unwrap();
    service.replay(journal.step(index)).unwrap();
    assert!(!service.runner.enabled);
    assert_eq!(journal.recover(&mut service).unwrap(), Outcome::Undone);
    assert!(service.runner.enabled);
}

#[cfg(unix)]
#[test]
fn interrupted_unit_move_restores_the_old_definition_and_its_link() {
    let fixture = cortexkit_test_support::ScratchDir::new("journal-unit-move");
    let root = fs::canonicalize(fixture.path()).unwrap();
    let paths = paths(&root);
    let old_dir = root.join("old");
    let wants = old_dir.join("default.target.wants");
    fs::create_dir_all(&wants).unwrap();
    let old = old_dir.join("cortexkit-subc.service");
    let old_link = wants.join("cortexkit-subc.service");
    fs::write(&old, OLD).unwrap();
    std::os::unix::fs::symlink(&old, &old_link).unwrap();
    let manifest = root.join("manifest");
    let mut inventory =
        crate::setup::inventory::Inventory::read(manifest.clone(), "linux-x64").unwrap();
    let semantics = crate::setup::definition::canonical(RuntimePlatform::Linux, OLD).unwrap();
    inventory.record(
        "runtime-definition",
        &old,
        Map::from_iter([("definition_semantics".into(), Value::String(semantics))]),
    );
    inventory.record("runtime-registration", &old, Map::new());
    inventory.save().unwrap();
    let mut runner = Manager {
        loaded: old.clone(),
        probes: vec![paths.definition.clone(), old.clone()],
        enabled: true,
        ..Manager::default()
    };
    let mut service = Service {
        platform: RuntimePlatform::Linux,
        paths: &paths,
        runner: &mut runner,
    };
    inventory.start_transaction().unwrap();
    move_unit(&mut service, &mut inventory, &old, NEW).unwrap();
    assert_eq!(service.runner.loaded, paths.definition);
    assert_eq!(fs::read_to_string(&paths.definition).unwrap(), NEW);
    assert!(!old.exists());
    assert!(!old_link.is_symlink());
    drop(inventory);
    assert_eq!(
        Journal::open(&manifest)
            .unwrap()
            .unwrap()
            .recover(&mut service)
            .unwrap(),
        Outcome::Undone
    );
    assert_eq!(service.runner.loaded, old);
    assert!(service.runner.enabled);
    assert_eq!(fs::read_to_string(&old).unwrap(), OLD);
    assert_eq!(fs::read_link(&old_link).unwrap(), old);
    assert!(!paths.definition.exists());
}

#[test]
fn launchd_requires_the_recorded_program_and_all_arguments() {
    let current = "program = /cmd/ck-subc\narguments = {\n\t/cmd/ck-subc\n\t--config-store-root\n\t/data/home with spaces\n}\n";
    let good = vec![
        "--config-store-root".into(),
        "/data/home with spaces".into(),
    ];
    assert!(launchd_arguments(current, Path::new("/cmd/ck-subc"), &good));
    assert!(!launchd_arguments(
        current,
        Path::new("/old/ck-subc"),
        &good
    ));
    assert!(!launchd_arguments(current, Path::new("/cmd/ck-subc"), &[]));
}
