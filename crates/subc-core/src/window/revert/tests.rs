use std::{
    cell::{Cell, RefCell},
    fs,
    path::{Path, PathBuf},
};

use super::{
    Environment, Liveness, Outcome, Owner, Record, RosterError, RuntimePlatform, StackOps,
    StackResult, StopIntent, WindowCheck, Windows,
};
use crate::{
    stack::{Survivor, SurvivorState},
    window::journal::{
        self, content, Action, Activation, Floor, Locked, Role, SnapshotTransfer, State, Step,
    },
};

const ID: &str = "19700101-000000-0001";

struct Rig {
    _root: cortexkit_test_support::ScratchDir,
    windows: Windows,
    work: PathBuf,
    dir: PathBuf,
    env: Fake,
}

#[derive(Default)]
struct Fake {
    events: RefCell<Vec<String>>,
    prompt: RefCell<Option<String>>,
    platform: Cell<Option<RuntimePlatform>>,
    refusal: Cell<Option<&'static str>>,
    lock_error: Cell<Option<&'static str>>,
    liveness: Cell<Option<Liveness>>,
    protection: Cell<Option<&'static str>>,
    roster: PathBuf,
    change_on_prompt: Change,
    change_on_stop: Change,
    change_before_start: Change,
    fail_stop: Cell<bool>,
    start_reply: RefCell<Option<StackResult>>,
    live: Cell<bool>,
    image_bad: Cell<bool>,
    undo_budget: Cell<Option<usize>>,
    fail_before_mark: Cell<bool>,
    fail_after_start: Cell<bool>,
    fail_integrity: Cell<bool>,
    fail_store_at: Cell<Option<&'static str>>,
    data: PathBuf,
}

type Change = RefCell<Option<Box<dyn FnOnce()>>>;

impl Fake {
    fn event(&self, name: &str) {
        self.events.borrow_mut().push(name.into());
    }
    fn happened(&self, name: &str) -> bool {
        self.events.borrow().iter().any(|event| event == name)
    }
}

impl Environment for Fake {
    fn platform(&self) -> RuntimePlatform {
        self.platform.get().unwrap_or(RuntimePlatform::Linux)
    }
    fn confirm(&self, text: &str) -> Result<(), String> {
        self.event("prompt");
        self.prompt.replace(Some(text.into()));
        if let Some(change) = self.change_on_prompt.take() {
            change();
        }
        self.refusal.get().map_or(Ok(()), |error| Err(error.into()))
    }
    fn lock(&self) -> Result<Box<dyn StackOps + '_>, String> {
        self.event("lock");
        self.lock_error
            .get()
            .map_or(Ok(Box::new(self)), |error| Err(error.into()))
    }
    fn owner(&self) -> Result<Owner, String> {
        let mut owner = Owner::current()?;
        // Model a revert process that has exited before the next invocation.
        owner.start_time += 1;
        Ok(owner)
    }
    fn liveness(&self, _owner: &Owner) -> Liveness {
        self.liveness.get().unwrap_or(Liveness::Gone)
    }
    fn recheck_protection(&self) -> Result<(), String> {
        self.event("protection");
        self.protection
            .get()
            .map_or(Ok(()), |error| Err(error.into()))
    }
    fn roster(&self) -> Result<PathBuf, RosterError> {
        self.event("roster");
        subc_daemon::read_live_children_roster(&self.roster)?;
        Ok(self.data.clone())
    }
    fn checkpoint(&self, point: &str) -> Result<(), String> {
        if self.fail_store_at.get() == Some(point) {
            self.fail_store_at.set(None);
            return Err(format!("interrupted at {point}"));
        }
        match point {
            "before-revert-start" => {
                if let Some(change) = self.change_before_start.take() {
                    change();
                }
            }
            "before-undo-mark" if self.fail_before_mark.replace(false) => {
                return Err("interrupted before undo mark".into())
            }
            "after-undo-mark" => {
                if let Some(left) = self.undo_budget.get() {
                    if left == 1 {
                        self.undo_budget.set(None);
                        return Err("interrupted after undo mark".into());
                    }
                    self.undo_budget.set(Some(left - 1));
                }
            }
            "after-start" if self.fail_after_start.replace(false) => {
                return Err("interrupted after start".into())
            }
            _ => {}
        }
        Ok(())
    }
    fn integrity(&self, path: &Path) -> Result<(), String> {
        self.event("integrity");
        if self.fail_integrity.get() {
            return Err("injected integrity_check failure".into());
        }
        super::store::integrity_check(path)
    }
}

impl StackOps for &Fake {
    fn stop(&self, intent: StopIntent) -> StackResult {
        self.event(if matches!(intent, StopIntent::Restart) {
            "stop/restart"
        } else {
            "stop"
        });
        if let Some(change) = self.change_on_stop.take() {
            change();
        }
        let mut report = StackResult::default();
        if self.fail_stop.replace(false) {
            report.exit = 1;
            report.survivors.push(Survivor {
                name: "worker".into(),
                pid: 123,
                start_time: Some(9),
                executable: None,
                state: SurvivorState::Alive,
                reason: None,
            });
        } else {
            self.live.set(false);
        }
        report
    }
    fn daemon_live(&self) -> Result<bool, Box<StackResult>> {
        self.event("probe");
        if self.image_bad.get() {
            return Err(Box::new(StackResult::refused("running image differs")));
        }
        Ok(self.live.get())
    }
    fn running_health(&self) -> StackResult {
        self.event("running-health");
        let mut report = StackResult::default();
        report.marker_removed = true;
        report
    }
    fn start(&self, window: &dyn Fn() -> WindowCheck) -> StackResult {
        assert_eq!(window(), WindowCheck::NoWindow);
        self.event("start");
        let report = self.start_reply.take().unwrap_or_default();
        self.live
            .set(report.exit == 0 || !report.unhealthy.is_empty());
        report
    }
}

impl Rig {
    fn new() -> Self {
        let root = cortexkit_test_support::ScratchDir::new("window-revert");
        let base = fs::canonicalize(&root).unwrap();
        let install = base.join("install");
        let work = base.join("work");
        let data = base.join("data");
        let dir = install.join("windows").join(ID);
        fs::create_dir_all(dir.join("saved")).unwrap();
        fs::create_dir(&work).unwrap();
        fs::create_dir(&data).unwrap();
        journal::create(&dir, ID, 0, Some("release".into())).unwrap();
        Self {
            _root: root,
            windows: Windows::at(&install, None).unwrap(),
            work,
            dir,
            env: Fake {
                roster: base.join("live-children.json"),
                data,
                ..Fake::default()
            },
        }
    }

    fn edit(&self) -> Locked {
        Locked::open(self.windows.root(), ID).unwrap()
    }

    fn step(
        &self,
        target: &Path,
        action: Action,
        module: Option<&str>,
        role: Option<Role>,
        done: bool,
    ) -> usize {
        let mut edit = self.edit();
        let index = edit.snapshot().steps.len() + 1;
        step(&mut edit, target, action, module, role, done);
        index
    }

    fn file(
        &self,
        target: &Path,
        prior: Option<&[u8]>,
        placed: Option<&[u8]>,
        module: Option<&str>,
        role: Option<Role>,
    ) -> usize {
        let i = self.edit().snapshot().steps.len() + 1;
        let saved = self.dir.join("saved").join(i.to_string());
        let stage = self.work.join("stage");
        let put = |path: &Path, bytes: Option<&[u8]>| {
            bytes.map(|bytes| {
                fs::write(path, bytes).unwrap();
                sha(path)
            })
        };
        let before = put(&saved, prior);
        let mode = content::file_info(&saved)
            .unwrap()
            .as_ref()
            .and_then(content::mode);
        let after = put(&stage, placed);
        let index = self.step(
            target,
            Action::File {
                before,
                after,
                mode,
            },
            module,
            role,
            true,
        );
        if let Some(placed) = placed {
            fs::write(target, placed).unwrap();
        } else if target.exists() {
            fs::remove_file(target).unwrap();
        }
        index
    }

    fn add_bundle(&self, target: &Path) -> usize {
        let i = self.edit().snapshot().steps.len() + 1;
        let saved = self.dir.join("saved").join(i.to_string());
        fs::create_dir(&saved).unwrap();
        fs::write(saved.join("image"), "prior").unwrap();
        fs::create_dir(target).unwrap();
        fs::write(target.join("image"), "placed").unwrap();
        self.step(
            target,
            Action::Bundle {
                before: content::bundle_digest(&saved).unwrap(),
                after: content::bundle_digest(target).unwrap().unwrap(),
            },
            None,
            None,
            true,
        )
    }

    fn add_store(&self, target: &Path, floors: &[(&str, &[u8])]) -> usize {
        self.add_store_with(target, floors, |snapshot| {
            let database = rusqlite::Connection::open(snapshot).unwrap();
            database
                .execute_batch("CREATE TABLE t (value TEXT); INSERT INTO t VALUES ('saved');")
                .unwrap();
        })
    }

    fn add_store_with(
        &self,
        target: &Path,
        floors: &[(&str, &[u8])],
        create: impl FnOnce(&Path),
    ) -> usize {
        let i = self.edit().snapshot().steps.len() + 1;
        let saved = self.dir.join("saved").join(i.to_string());
        fs::create_dir(&saved).unwrap();
        let snapshot = saved.join("snapshot");
        create(&snapshot);
        let floors = floors
            .iter()
            .map(|(name, bytes)| {
                let path = saved.join("floors").join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, bytes).unwrap();
                Floor {
                    path: PathBuf::from(name),
                    sha256: sha(&path),
                }
            })
            .collect();
        self.step(
            target,
            Action::Store {
                snapshot_sha256: sha(&snapshot),
                snapshot_transfer: SnapshotTransfer::Copied,
                floors,
            },
            Some("worker"),
            None,
            true,
        )
    }

    fn activation(&self) {
        self.step(
            Path::new("daemon"),
            Action::Activation {
                before: Activation::Daemon { process: None },
            },
            None,
            None,
            true,
        );
    }

    fn run(&self) -> super::RevertResult {
        self.windows.revert_with(None, &self.env)
    }

    fn expect(&self, outcome: Outcome, total: usize, undone: usize) -> super::RevertResult {
        let result = self.run();
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["schema"], "ck.window.result/v1");
        assert_eq!(
            json["outcome"],
            serde_json::to_value(outcome).unwrap(),
            "{}",
            result.human()
        );
        assert_eq!(json["exit"], outcome.exit());
        assert_eq!(json["steps_total"], total);
        assert_eq!(json["steps_undone"], undone);
        result
    }
}

fn step(
    edit: &mut Locked,
    target: &Path,
    action: Action,
    module: Option<&str>,
    role: Option<Role>,
    done: bool,
) {
    let index = edit.snapshot().steps.len() + 1;
    edit.append(Record::Step {
        step: Box::new(Step {
            target: target.to_owned(),
            action,
            owner: Owner::current().unwrap(),
            module: module.map(str::to_owned),
            role,
            config_generation: None,
        }),
    })
    .unwrap();
    if done {
        edit.append(Record::Done { index }).unwrap();
    }
}

fn sha(path: &Path) -> String {
    content::file_digest(path).unwrap().unwrap()
}
fn put(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn restores_all_step_kinds_in_reverse_then_starts_and_marks_activation() {
    let rig = Rig::new();
    let replaced = rig.work.join("replaced");
    let new = rig.work.join("new");
    let removed = rig.work.join("removed");
    let store = rig.work.join("data.sqlite");
    let bundle = rig.work.join("Daemon.app");
    rig.file(&replaced, Some(b"before"), Some(b"after"), None, None);
    rig.file(&new, None, Some(b"new"), None, None);
    rig.file(&removed, Some(b"removed"), None, None, None);
    rig.add_store(&store, &[]);
    put(&store, b"post-window bytes");
    rig.add_bundle(&bundle);
    rig.activation();
    let mut unhealthy = StackResult::default();
    unhealthy.exit = 1;
    unhealthy.unhealthy.push("worker".into());
    unhealthy.not_health_checked.push("observer".into());
    rig.env.start_reply.replace(Some(unhealthy));
    let report = rig.expect(Outcome::Reverted, 6, 6);
    assert!(report.human().contains("Unhealthy: worker"));
    assert!(report
        .human()
        .contains("Running, not health-checked: observer"));
    assert_eq!(fs::read(replaced).unwrap(), b"before");
    assert!(!new.exists());
    assert_eq!(fs::read(removed).unwrap(), b"removed");
    assert_eq!(fs::read(bundle.join("image")).unwrap(), b"prior");
    assert_eq!(fs::read(rig.dir.join("aside/5/image")).unwrap(), b"placed");
    assert_eq!(
        fs::read(rig.dir.join("aside/4/1/files/data.sqlite")).unwrap(),
        b"post-window bytes"
    );
    assert!(rig.env.happened("stop/restart"));
    assert!(rig.env.happened("start"));
    let events = fs::read_to_string(rig.dir.join("log.txt")).unwrap();
    let undo_events: Vec<_> = events
        .lines()
        .filter(|line| {
            line.contains(" undo ") || line.contains(" restored ") || line.contains(" revert-end ")
        })
        .map(|line| line.split_whitespace().nth(3).unwrap())
        .collect();
    assert_eq!(
        undo_events,
        ["5/6", "4/6", "3/6", "2/6", "1/6", "-", "6/6", "-"]
    );
    let list = rig.windows.list().unwrap();
    assert_eq!(list.windows[0].revert_status, Some("reverted"));
}

#[test]
fn stop_failure_and_two_undos_report_durable_counts_then_resume() {
    let rig = Rig::new();
    for i in 1..=4 {
        rig.file(
            &rig.work.join(i.to_string()),
            Some(b"before"),
            Some(b"after"),
            None,
            None,
        );
    }
    rig.env.fail_stop.set(true);
    let stopped = rig.expect(Outcome::StoppedPartWay, 4, 0);
    assert!(stopped.human().contains("worker (pid 123"));
    assert!(!rig.env.happened("start"));
    let list = rig.windows.list().unwrap();
    assert_eq!(list.windows[0].state, State::RevertInterrupted);
    assert_eq!(list.windows[0].revert_status, Some("stopped_part_way"));
    assert_eq!(fs::read(rig.work.join("4")).unwrap(), b"after");
    rig.env.undo_budget.set(Some(2));
    rig.expect(Outcome::StoppedPartWay, 4, 2);
    fs::write(rig.work.join("2"), "other").unwrap();
    rig.expect(Outcome::RefusedBeforeChange, 4, 2);
    assert_eq!(fs::read(rig.work.join("2")).unwrap(), b"other");
    fs::write(rig.work.join("2"), "after").unwrap();
    rig.expect(Outcome::Reverted, 4, 4);
}

#[test]
fn reverse_walk_handles_two_placements_same_target_and_repeated_bytes() {
    for newest in [b"A", b"C"] {
        let rig = Rig::new();
        let file = rig.work.join("target");
        rig.file(&file, Some(b"A"), Some(b"B"), None, None);
        rig.file(&file, Some(b"B"), Some(newest), None, None);
        rig.env.undo_budget.set(Some(1));
        rig.expect(Outcome::StoppedPartWay, 2, 1);
        assert_eq!(fs::read(&file).unwrap(), b"B");
        rig.expect(Outcome::Reverted, 2, 2);
        assert_eq!(fs::read(file).unwrap(), b"A");
    }
}

#[test]
fn refused_presence_bad_rosters_and_protection_do_not_stop_or_append() {
    for error in ["Declined", "AuthorizedWithoutPrompt", "UnsupportedPlatform"] {
        let rig = Rig::new();
        rig.env.refusal.set(Some(error));
        let report = rig.expect(Outcome::RefusedBeforeChange, 0, 0);
        assert!(report.human().contains(error));
        assert_eq!(*rig.env.events.borrow(), ["prompt"]);
        assert_eq!(rig.edit().snapshot().record_count, 1);
    }
    for (bytes, expected) in [
        (Some("{"), "malformed"),
        (Some(r#"{"version":19,"children":[]}"#), "unknown_version"),
        (None, "unreadable"),
    ] {
        let rig = Rig::new();
        if let Some(bytes) = bytes {
            put(&rig.env.roster, bytes);
        } else {
            fs::create_dir(&rig.env.roster).unwrap();
        }
        let report = rig.expect(Outcome::RefusedBeforeChange, 0, 0);
        assert_eq!(
            serde_json::to_value(report).unwrap()["roster_error"]["kind"],
            expected
        );
        assert!(!rig.env.happened("stop/restart"));
        assert_eq!(rig.edit().snapshot().record_count, 1);
    }
    let rig = Rig::new();
    rig.env
        .protection
        .set(Some("protected installs are not covered"));
    let report = rig.expect(Outcome::RefusedBeforeChange, 0, 0);
    assert!(report.human().contains("protected installs"));
    assert!(!rig.env.happened("stop/restart"));
}

#[test]
fn prompt_names_window_note_count_and_platform_and_precedes_lock() {
    for (platform, intent) in [
        (
            RuntimePlatform::Linux,
            "Modules will be told the stack is restarting and may keep long-running work.",
        ),
        (
            RuntimePlatform::Macos,
            "Modules will be told the stack is restarting and may keep long-running work.",
        ),
        (
            RuntimePlatform::Windows,
            "Modules will be told to stop and give up long-running work.",
        ),
    ] {
        let rig = Rig::new();
        rig.activation();
        rig.env.platform.set(Some(platform));
        rig.env
            .lock_error
            .set(Some("ck upgrade holds installer.lock"));
        let result = rig.expect(Outcome::RefusedBeforeChange, 1, 0);
        assert!(result.human().contains("ck upgrade holds installer.lock"));
        assert_eq!(rig.env.prompt.borrow().as_deref(), Some(format!("Allow `ck window revert` to restore window {ID} (note \"release\"; 1 of 1 steps remaining) and restart the stack? {intent}").as_str()));
        assert_eq!(*rig.env.events.borrow(), ["prompt", "lock"]);
    }
}

#[test]
fn count_is_checked_after_prompt_and_again_immediately_before_revert_start() {
    for while_prompting in [true, false] {
        let rig = Rig::new();
        let journal = rig.dir.join("journal");
        let root = rig.windows.root().to_owned();
        let mutate = Box::new(move || {
            if while_prompting {
                step(
                    &mut Locked::open(&root, ID).unwrap(),
                    Path::new("daemon"),
                    Action::Activation {
                        before: Activation::Daemon { process: None },
                    },
                    None,
                    None,
                    true,
                );
            } else {
                use std::io::Write as _;
                let mut file = fs::OpenOptions::new().append(true).open(&journal).unwrap();
                let step = Record::Step {
                    step: Box::new(Step {
                        target: "daemon".into(),
                        action: Action::Activation {
                            before: Activation::Daemon { process: None },
                        },
                        owner: Owner::current().unwrap(),
                        module: None,
                        role: None,
                        config_generation: None,
                    }),
                };
                writeln!(file, "{}", serde_json::to_string(&step).unwrap()).unwrap();
            }
        });
        if while_prompting {
            rig.env.change_on_prompt.replace(Some(mutate));
        } else {
            rig.env.change_before_start.replace(Some(mutate));
        }
        let report = rig.expect(Outcome::RefusedBeforeChange, 1, 0);
        assert!(report.human().contains("changed"));
        assert!(rig.edit().snapshot().revert_owner.is_none());
        assert!(!rig.env.happened("stop/restart"));
    }
}

#[test]
fn preflight_checks_old_saved_copy_before_stop_even_when_newer_step_is_a_noop() {
    let rig = Rig::new();
    let a = rig.work.join("a");
    let b = rig.work.join("b");
    rig.file(&a, Some(b"before"), Some(b"after"), None, None);
    rig.file(&b, Some(b"same"), Some(b"same"), None, None);
    put(&rig.dir.join("saved/1"), "corrupt");
    let report = rig.expect(Outcome::RefusedBeforeChange, 2, 0);
    assert!(report.human().contains("expected sha256"));
    assert!(!rig.env.happened("stop/restart"));
    assert!(rig.edit().snapshot().revert_owner.is_none());
    assert_eq!(rig.windows.list().unwrap().windows[0].revert_status, None);
    assert_eq!(fs::read(a).unwrap(), b"after");
}

#[test]
fn install_journal_and_unreadable_owners_are_refused() {
    let rig = Rig::new();
    let journal = super::Journal::path(&rig.windows.root().join("installer-manifest.json"));
    put(&journal, "installer owns this");
    assert!(rig
        .expect(Outcome::RefusedBeforeChange, 0, 0)
        .human()
        .contains("installer journal"));
    assert_eq!(fs::read(&journal).unwrap(), b"installer owns this");
    fs::remove_file(journal).unwrap();
    rig.step(
        Path::new("daemon"),
        Action::Activation {
            before: Activation::Daemon { process: None },
        },
        None,
        None,
        false,
    );
    rig.env.liveness.set(Some(Liveness::Unreadable));
    let report = rig.expect(Outcome::RefusedBeforeChange, 1, 0);
    assert!(report.human().contains("step 1 is busy"));
    assert!(report.human().contains("identity unreadable"));
    rig.env.liveness.set(Some(Liveness::Gone));
    rig.env.fail_stop.set(true);
    rig.expect(Outcome::StoppedPartWay, 1, 0);
    rig.env.liveness.set(Some(Liveness::Unreadable));
    let report = rig.expect(Outcome::RefusedBeforeChange, 1, 0);
    assert!(report.human().contains("is reverting; revert owner"));
    assert!(report.human().contains("identity unreadable"));
}

#[test]
fn changed_target_during_stop_does_not_get_overwritten() {
    let rig = Rig::new();
    let file = rig.work.join("target");
    rig.file(&file, Some(b"before"), Some(b"after"), None, None);
    let target = file.clone();
    rig.env
        .change_on_stop
        .replace(Some(Box::new(move || put(&target, "outside change"))));
    rig.expect(Outcome::StoppedPartWay, 1, 0);
    assert_eq!(fs::read(file).unwrap(), b"outside change");
    assert!(!rig.env.happened("start"));
}

#[cfg(unix)]
#[test]
fn symlink_target_and_absent_target_with_symlink_parent_refuse() {
    use std::os::unix::fs::symlink;
    for parent in [false, true] {
        let rig = Rig::new();
        let directory = rig.work.join("original");
        let real = rig.work.join("real");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(&real).unwrap();
        let file = directory.join("file");
        let other = real.join("file");
        put(&other, "same bytes");
        if parent {
            rig.file(&file, Some(b"before"), None, None, None);
            fs::remove_file(&other).unwrap();
            fs::remove_dir(&directory).unwrap();
            symlink(&real, &directory).unwrap();
        } else {
            rig.file(&file, Some(b"before"), Some(b"same bytes"), None, None);
            fs::remove_file(&file).unwrap();
            symlink(&other, &file).unwrap();
        }
        rig.expect(Outcome::RefusedBeforeChange, 1, 0);
        assert!(!rig.env.happened("stop/restart"));
    }
}

#[cfg(unix)]
#[test]
fn prior_file_with_the_wrong_mode_is_restored_not_skipped() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    let file = rig.work.join("target");
    let saved = rig.dir.join("saved/1");
    put(&file, b"same bytes");
    put(&saved, b"same bytes");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    rig.step(
        &file,
        Action::File {
            before: Some(sha(&saved)),
            after: Some(sha(&file)),
            mode: Some(0o751),
        },
        None,
        None,
        true,
    );
    rig.expect(Outcome::Reverted, 1, 1);
    assert_eq!(content::mode(&fs::metadata(file).unwrap()), Some(0o751));
}

#[test]
fn recheck_after_last_undo_and_after_restored_refuses_a_replaced_file() {
    for restored in [false, true] {
        let rig = Rig::new();
        let bin = rig.work.join("ck-subc");
        rig.file(&bin, Some(b"before"), Some(b"after"), None, None);
        rig.activation();
        if restored {
            rig.env
                .start_reply
                .replace(Some(StackResult::refused("start failure")));
        } else {
            rig.env.undo_budget.set(Some(1));
        }
        rig.expect(Outcome::StoppedPartWay, 2, 1);
        assert_eq!(rig.edit().snapshot().restored.is_some(), restored);
        put(&bin, "replacement");
        rig.env.events.borrow_mut().clear();
        rig.expect(Outcome::StoppedPartWay, 2, 1);
        assert!(!rig.env.happened("start"));
        assert_eq!(fs::read(bin).unwrap(), b"replacement");
    }
}

#[test]
fn manager_failure_and_daemon_unreachable_retry_only_start() {
    for unreachable in [false, true] {
        let rig = Rig::new();
        let bin = rig.work.join("image");
        rig.file(&bin, Some(b"before"), Some(b"after"), None, None);
        rig.activation();
        let mut failure = StackResult::default();
        failure.exit = 1;
        failure.daemon_unreachable = unreachable;
        failure.manager_error = (!unreachable).then(|| "manager refused start".into());
        rig.env.start_reply.replace(Some(failure));
        let report = rig.expect(Outcome::StoppedPartWay, 2, 1);
        assert_eq!(report.report.daemon_unreachable, unreachable);
        assert!(rig.edit().snapshot().restored.is_some());
        rig.env.events.borrow_mut().clear();
        rig.expect(Outcome::Reverted, 2, 2);
        assert!(rig.env.happened("start"));
        assert!(!rig.env.happened("stop/restart"));
    }
}

#[test]
fn after_restored_a_live_daemon_and_a_module_write_follow_platform_rules() {
    for platform in [
        RuntimePlatform::Linux,
        RuntimePlatform::Macos,
        RuntimePlatform::Windows,
    ] {
        let rig = Rig::new();
        rig.env.platform.set(Some(platform));
        let store = rig.work.join("db.sqlite");
        rig.add_store(&store, &[]);
        put(&store, "before revert");
        rig.activation();
        rig.env.fail_after_start.set(true);
        rig.expect(Outcome::StoppedPartWay, 2, 1);
        put(&store, "module write after restart");
        if platform == RuntimePlatform::Linux {
            rig.env.image_bad.set(true);
            rig.env.events.borrow_mut().clear();
            rig.expect(Outcome::StoppedPartWay, 2, 1);
            assert!(!rig.env.happened("stop/restart"));
            assert!(!rig.env.happened("start"));
            rig.env.image_bad.set(false);
        }
        rig.env.events.borrow_mut().clear();
        let report = rig.expect(Outcome::Reverted, 2, 2);
        assert_eq!(
            report.report.marker_removed,
            platform == RuntimePlatform::Linux
        );
        assert_eq!(
            rig.env.happened("running-health"),
            platform == RuntimePlatform::Linux
        );
        assert_eq!(
            rig.env.happened("stop/restart"),
            platform != RuntimePlatform::Linux
        );
        assert_eq!(
            rig.env.happened("start"),
            platform != RuntimePlatform::Linux
        );
        assert_eq!(fs::read(store).unwrap(), b"module write after restart");
    }
}

#[test]
fn resumes_after_bundle_set_aside_and_after_new_file_removed_without_mark() {
    let rig = Rig::new();
    let bundle = rig.work.join("Daemon.app");
    rig.add_bundle(&bundle);
    let new_file = rig.work.join("new");
    rig.file(&new_file, None, Some(b"placed"), None, None);
    rig.env.fail_before_mark.set(true);
    rig.expect(Outcome::StoppedPartWay, 2, 0);
    assert!(!new_file.exists());
    fs::create_dir(rig.dir.join("aside")).unwrap();
    let aside = rig.dir.join("aside/1");
    fs::rename(&bundle, &aside).unwrap();
    let unrelated = rig.work.join("unrelated/restore.partial");
    put(&unrelated, "not owned by revert");
    rig.expect(Outcome::Reverted, 2, 2);
    assert_eq!(fs::read(aside.join("image")).unwrap(), b"placed");
    assert_eq!(fs::read(bundle.join("image")).unwrap(), b"prior");
    assert_eq!(fs::read(unrelated).unwrap(), b"not owned by revert");
}

const FLOOR_1: &[u8] = br#"{"version":1,"floors":{"records":1}}"#;
const FLOOR_2: &[u8] = br#"{"version":1,"floors":{"records":2}}"#;
const MAP_1: &[u8] = br#"{"records":1}"#;
const MAP_2: &[u8] = br#"{"records":2}"#;

#[test]
fn floor_projection_uses_old_map_and_saved_floors_without_masking_other_trees() {
    // Each case varies which data tree still carries a floor of two.
    for (store_module, second_tree, saved_floor, expected) in [
        ("other", false, FLOOR_1, Outcome::RefusedBeforeChange),
        ("worker", true, FLOOR_1, Outcome::RefusedBeforeChange),
        ("worker", false, FLOOR_2, Outcome::RefusedBeforeChange),
        ("worker", false, FLOOR_1, Outcome::Reverted),
    ] {
        let rig = Rig::new();
        let bin = rig.work.join("worker");
        let map = rig.work.join("worker.format-versions.json");
        rig.file(
            &bin,
            Some(b"prior"),
            Some(b"placed"),
            Some("worker"),
            Some(Role::Binary),
        );
        rig.file(
            &map,
            Some(MAP_1),
            Some(MAP_2),
            Some("worker"),
            Some(Role::FormatMap),
        );
        let store_tree = rig.env.data.join(store_module).join("store");
        let store = store_tree.join("db.sqlite");
        put(&store, "post-window write");
        let deleted = "old/format-floor.json";
        rig.add_store(&store, &[(deleted, saved_floor)]);
        let created = store_tree.join("new/format-floor.json");
        put(&created, FLOOR_2);
        if second_tree || store_module != "worker" {
            put(
                &rig.env.data.join("worker/other/format-floor.json"),
                FLOOR_2,
            );
        }
        let result = rig.expect(
            expected,
            3,
            if expected == Outcome::Reverted { 3 } else { 0 },
        );
        if expected == Outcome::Reverted {
            assert_eq!(fs::read(store_tree.join(deleted)).unwrap(), FLOOR_1);
            assert!(!created.exists());
            assert_eq!(fs::read(map).unwrap(), MAP_1);
        } else {
            assert!(result.human().contains("records floor 2"));
            assert!(!rig.env.happened("stop/restart"));
            assert_eq!(fs::read(map).unwrap(), MAP_2);
            assert_eq!(fs::read(store).unwrap(), b"post-window write");
        }
    }
}

#[test]
fn floor_check_runs_again_after_stop_and_honors_four_level_limit() {
    let rig = Rig::new();
    let bin = rig.work.join("SIGNED.worker");
    let fallback = rig.work.join("worker.format-versions.json");
    put(&fallback, MAP_1);
    rig.file(
        &bin,
        Some(b"prior"),
        Some(b"placed"),
        Some("worker"),
        Some(Role::Binary),
    );
    let tree = rig.env.data.join("worker");
    put(&tree.join("a/b/c/d/format-floor.json"), FLOOR_2);
    put(&tree.join("a/b/c/format-floor.json"), FLOOR_1);
    rig.env.change_on_stop.replace(Some(Box::new(move || {
        put(&tree.join("a/b/c/format-floor.json"), FLOOR_2)
    })));
    rig.expect(Outcome::StoppedPartWay, 1, 0);
    assert_eq!(fs::read(bin).unwrap(), b"placed");
    assert!(!rig.env.happened("start"));
}

#[test]
fn snapshot_reads_a_committed_uncheckpointed_row_and_restores_sidecars_and_floors() {
    for suffixes in [&["-wal", "-shm"][..], &["-journal"][..]] {
        let rig = Rig::new();
        let store = rig.work.join("data/db.sqlite");
        put(&store, "post-window store");
        for &suffix in suffixes {
            put(&super::files::suffix(&store, suffix), suffix);
        }
        let floor = store.with_file_name("format-floor.json");
        put(&floor, FLOOR_2);
        let source = rig.work.join("source.sqlite");
        let live = rusqlite::Connection::open(&source).unwrap();
        live.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t (value TEXT); INSERT INTO t VALUES ('only in WAL');").unwrap();
        assert!(
            fs::metadata(super::files::suffix(&source, "-wal"))
                .unwrap()
                .len()
                > 0
        );
        rig.add_store_with(&store, &[("format-floor.json", FLOOR_1)], |saved| {
            live.execute("VACUUM INTO ?1", [saved.to_str().unwrap()])
                .unwrap();
        });
        rig.expect(Outcome::Reverted, 1, 1);
        let placed = rusqlite::Connection::open(&store).unwrap();
        let value: String = placed
            .query_row("SELECT value FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "only in WAL");
        drop(placed);
        for &suffix in suffixes {
            assert!(!super::files::suffix(&store, suffix).exists());
            assert_eq!(
                fs::read(rig.dir.join(format!("aside/1/1/files/db.sqlite{suffix}"))).unwrap(),
                suffix.as_bytes()
            );
        }
        assert_eq!(
            fs::read(rig.dir.join("aside/1/1/files/db.sqlite")).unwrap(),
            b"post-window store"
        );
        assert_eq!(fs::read(&floor).unwrap(), FLOOR_1);
        assert_eq!(
            fs::read(rig.dir.join("aside/1/1/files/format-floor.json")).unwrap(),
            FLOOR_2
        );
    }
}

#[test]
fn cross_volume_retry_repairs_a_published_copy_without_opening_another_attempt() {
    use journal::durable::{CROSS_DEVICE_FROM, EVENTS, FAIL_SYNC};
    let rig = Rig::new();
    let store = rig.work.join("db.sqlite");
    put(&store, "before");
    rig.add_store(&store, &[]);
    let parent = rig.dir.join("aside/1/1");
    let final_name = parent.join("files/db.sqlite");
    let partial = super::files::suffix(&final_name, ".partial");
    CROSS_DEVICE_FROM.with(|it| it.replace(Some(store.clone())));
    FAIL_SYNC.with(|it| it.replace(Some(partial.clone())));
    EVENTS.with(|it| it.borrow_mut().clear());
    rig.expect(Outcome::StoppedPartWay, 1, 0);
    assert_eq!(fs::read(&store).unwrap(), b"before");
    assert!(!final_name.exists());
    EVENTS.with(|it| {
        let events = it.borrow();
        let manifest_sync = events
            .iter()
            .position(|(kind, path)| *kind == "file" && path == &parent.join("members.partial"))
            .unwrap();
        let manifest_dir = events
            .iter()
            .position(|(kind, path)| *kind == "parent" && path == &parent.join("members"))
            .unwrap();
        assert!(manifest_sync < manifest_dir);
    });
    // Model a crash after the copy has become durable but before source removal.
    journal::durable::rename_new(&partial, &final_name).unwrap();
    rig.expect(Outcome::Reverted, 1, 1);
    CROSS_DEVICE_FROM.with(|it| it.take());
    assert_eq!(fs::read(final_name).unwrap(), b"before");
    assert!(!rig.dir.join("aside/1/2").exists());
    assert_ne!(fs::read(store).unwrap(), b"before");
}

#[test]
fn interrupted_after_one_sidecar_uses_the_same_attempt_for_integrity_rollback() {
    use journal::durable::{CROSS_DEVICE_FROM, FAIL_SYNC};
    let rig = Rig::new();
    let store = rig.work.join("db.sqlite");
    let wal = super::files::suffix(&store, "-wal");
    put(&store, "original store");
    put(&wal, "original wal");
    rig.add_store(&store, &[]);
    let attempt = rig.dir.join("aside/1/1");
    CROSS_DEVICE_FROM.with(|it| it.replace(Some(store.clone())));
    FAIL_SYNC.with(|it| it.replace(Some(attempt.join("files/db.sqlite.partial"))));
    rig.expect(Outcome::StoppedPartWay, 1, 0);
    CROSS_DEVICE_FROM.with(|it| it.take());
    assert!(!wal.exists());
    assert_eq!(fs::read(&store).unwrap(), b"original store");
    rig.env.fail_integrity.set(true);
    rig.expect(Outcome::StoppedPartWay, 1, 0);
    assert_eq!(fs::read(&wal).unwrap(), b"original wal");
    assert_eq!(fs::read(&store).unwrap(), b"original store");
    assert!(attempt.join("rollback").is_file());
    assert_eq!(
        fs::read(attempt.join("files/db.sqlite-wal")).unwrap(),
        b"original wal"
    );
    assert!(!rig.dir.join("aside/1/2").exists());
    assert!(!rig.env.happened("start"));
    rig.env.fail_integrity.set(false);
    rig.expect(Outcome::Reverted, 1, 1);
    assert!(rig.dir.join("aside/1/2/members").is_file());
    assert_eq!(
        fs::read(attempt.join("files/db.sqlite-wal")).unwrap(),
        b"original wal"
    );
}

#[test]
fn interrupted_store_checks_integrity_before_undo_and_rolls_back_exactly() {
    let rig = Rig::new();
    let store = rig.work.join("db.sqlite");
    let old_floor = "original/format-floor.json";
    let saved_floor = "saved/format-floor.json";
    put(&store, "original store");
    put(&rig.work.join(old_floor), FLOOR_2);
    let unrelated = rig.work.join("unrelated");
    put(&unrelated, "leave me");
    rig.add_store(&store, &[(saved_floor, FLOOR_1)]);
    for fault in [
        "after-store-set-aside",
        "after-store-floor",
        "before-store-integrity",
    ] {
        rig.env.fail_store_at.set(Some(fault));
        rig.expect(Outcome::StoppedPartWay, 1, 0);
        assert!(!rig.env.happened("integrity"));
        assert!(!rig.work.join(old_floor).exists());
    }
    rig.env.fail_integrity.set(true);
    rig.expect(Outcome::StoppedPartWay, 1, 0);
    assert!(rig.env.happened("integrity"));
    assert!(!rig.env.happened("start"));
    assert_eq!(fs::read(store).unwrap(), b"original store");
    assert_eq!(fs::read(rig.work.join(old_floor)).unwrap(), FLOOR_2);
    assert_eq!(
        fs::read(rig.dir.join("aside/1/1/files").join(old_floor)).unwrap(),
        FLOOR_2
    );
    assert!(!rig.work.join(saved_floor).exists());
    assert_eq!(fs::read(unrelated).unwrap(), b"leave me");
}

#[test]
fn resumed_store_after_restore_before_undo_mark_keeps_the_original_aside() {
    let rig = Rig::new();
    let store = rig.work.join("db.sqlite");
    put(&store, "before");
    rig.add_store(&store, &[]);
    rig.env.fail_before_mark.set(true);
    rig.expect(Outcome::StoppedPartWay, 1, 0);
    rig.expect(Outcome::Reverted, 1, 1);
    assert_eq!(
        fs::read(rig.dir.join("aside/1/1/files/db.sqlite")).unwrap(),
        b"before"
    );
    assert!(!rig.dir.join("aside/1/2").exists());
    assert_eq!(
        rig.env
            .events
            .borrow()
            .iter()
            .filter(|event| event.as_str() == "integrity")
            .count(),
        2
    );
}

#[test]
fn unresolved_revert_omits_counts() {
    let rig = Rig::new();
    let report = rig.windows.revert_with(Some("no-such-window"), &rig.env);
    let json = serde_json::to_value(report).unwrap();
    assert_eq!(json["outcome"], "refused_before_change");
    assert_eq!(json["exit"], 2);
    assert!(json.get("steps_total").is_none());
    assert!(json.get("steps_undone").is_none());
}

#[test]
fn exclusive_publication_preserves_an_existing_aside_entry() {
    let rig = Rig::new();
    let from = rig.work.join("staged");
    let to = rig.work.join("aside");
    put(&from, "candidate");
    put(&to, "original");
    assert!(super::files::move_new(&from, &to).is_err());
    assert_eq!(fs::read(from).unwrap(), b"candidate");
    assert_eq!(fs::read(to).unwrap(), b"original");
}

#[cfg(target_os = "macos")]
#[test]
fn restored_macos_bundle_keeps_its_code_signature() {
    use std::process::Command;

    let rig = Rig::new();
    let target = rig.work.join("Daemon.app");
    let saved = rig.dir.join("saved/1");
    let old_bundle = rig.work.join("Original.app");
    for (dir, version) in [(&old_bundle, "1"), (&target, "2")] {
        let contents = dir.join("Contents");
        fs::create_dir_all(contents.join("MacOS")).unwrap();
        fs::copy("/usr/bin/true", contents.join("MacOS/daemon")).unwrap();
        put(
            &contents.join("Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleExecutable</key><string>daemon</string>
<key>CFBundleIdentifier</key><string>example.daemon</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>{version}</string></dict></plist>"#
            ),
        );
        let signed = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(dir)
            .output()
            .unwrap();
        assert!(
            signed.status.success(),
            "{}",
            String::from_utf8_lossy(&signed.stderr)
        );
    }
    fs::rename(old_bundle, &saved).unwrap();
    rig.step(
        &target,
        Action::Bundle {
            before: content::bundle_digest(&saved).unwrap(),
            after: content::bundle_digest(&target).unwrap().unwrap(),
        },
        None,
        None,
        true,
    );
    rig.expect(Outcome::Reverted, 1, 1);
    let verified = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(target)
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
}
