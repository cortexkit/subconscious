use super::*;

mod crash;

#[test]
fn a_failed_append_truncates_unsynced_bytes_and_poisons_the_handle() {
    for fail_after_full_write in [false, true] {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let path = Journal::path(&fixture.manifest());
        let previous = fs::read(&path).unwrap();
        let error = journal
            .append_with(
                Record::Undo {
                    phase: Undo::Quiesce,
                },
                |file, bytes| {
                    file.write_all(if fail_after_full_write {
                        bytes
                    } else {
                        &bytes[..5]
                    })?;
                    Err(io::Error::other("injected journal I/O failure"))
                },
            )
            .unwrap_err();
        assert!(error.contains("injected journal I/O failure"));
        assert_eq!(fs::read(&path).unwrap(), previous);
        assert_eq!(journal.state.undo, None);
        let retry = journal
            .append(Record::Undo {
                phase: Undo::Quiesce,
            })
            .unwrap_err();
        assert!(retry.contains("reopen"), "{retry}");
        drop(journal);
        assert!(Journal::open(&fixture.manifest())
            .unwrap()
            .unwrap()
            .state
            .undo
            .is_none());
    }
}

struct Fixture {
    directory: cortexkit_test_support::ScratchDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            directory: cortexkit_test_support::ScratchDir::new("install-journal"),
        }
    }

    fn manifest(&self) -> PathBuf {
        self.directory.path().join("installer-manifest.json")
    }

    fn target(&self) -> PathBuf {
        self.directory.path().join("daemon.conf")
    }

    fn journal(&self) -> Journal {
        Journal::create(&self.manifest()).unwrap()
    }

    fn recover(&self, backend: &mut Files) -> Result<Outcome, String> {
        Journal::open(&self.manifest())
            .unwrap()
            .unwrap()
            .recover(backend)
    }
}

#[derive(Default)]
struct Files {
    interrupt_after_undo: Option<PathBuf>,
}

fn bytes(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn write(path: &Path, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        fs::write(path, value).map_err(|error| error.to_string())
    } else {
        fs::remove_file(path).map_err(|error| error.to_string())
    }
}

impl Recovery for Files {
    fn is_applied(&mut self, step: &Step) -> Result<bool, String> {
        Ok(bytes(&step.target)? == step.action.file().unwrap().after)
    }

    fn can_replay(&mut self, _id: &str, step: &Step) -> Result<bool, String> {
        Ok(bytes(&step.target)? == step.action.file().unwrap().before)
    }

    fn replay(&mut self, _id: &str, step: &Step) -> Result<(), String> {
        write(&step.target, step.action.file().unwrap().after.as_deref())
    }

    fn undo(&mut self, _id: &str, phase: Undo, step: &Step) -> Result<(), String> {
        if phase != Undo::Restore {
            return Ok(());
        }
        let file = step.action.file().unwrap();
        let actual = bytes(&step.target)?;
        if actual == file.before {
            return Ok(());
        }
        if actual != file.after {
            return Err(format!("unexpected bytes at {}", step.target.display()));
        }
        write(&step.target, file.before.as_deref())?;
        if self.interrupt_after_undo.as_ref() == Some(&step.target) {
            return Err("interrupted after restoring bytes and before recording undo".into());
        }
        Ok(())
    }

    fn prepare_discard(
        &mut self,
        _id: &str,
        _step: &Step,
        _outcome: Outcome,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn discard(
        &mut self,
        _id: &str,
        _step: &Step,
        _path: &Path,
        _identity: &str,
    ) -> Result<Vec<PathBuf>, crate::setup::install_journal::CleanupRefusal> {
        Ok(Vec::new())
    }
}

fn step(path: &Path, before: Option<&str>, after: &str) -> Step {
    Step {
        target: path.to_path_buf(),
        action: Action::File(FileStep {
            before: before.map(str::to_owned),
            after: Some(after.into()),
            previous: None,
            staged: None,
            staging_directory: path.with_extension("stage"),
        }),
    }
}

fn append_torn(manifest: &Path) {
    let path = Journal::path(manifest);
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(br#"{"record":"seal","step":{"target":"installer"#)
        .unwrap();
}

#[test]
fn a_kill_after_write_ahead_leaves_the_old_file() {
    let fixture = Fixture::new();
    let target = fixture.target();
    fs::write(&target, "before").unwrap();
    let mut journal = fixture.journal();
    journal
        .prepare(step(&target, Some("before"), "new"))
        .unwrap();
    drop(journal);
    assert_eq!(
        fixture.recover(&mut Files::default()).unwrap(),
        Outcome::Undone
    );
    assert_eq!(bytes(&target).unwrap().as_deref(), Some("before"));
    assert!(!Journal::exists(&fixture.manifest()).unwrap());
}

#[test]
fn a_kill_after_acting_without_a_done_mark_undoes_the_file() {
    let fixture = Fixture::new();
    let target = fixture.target();
    fs::write(&target, "before").unwrap();
    let mut journal = fixture.journal();
    journal
        .prepare(step(&target, Some("before"), "new"))
        .unwrap();
    fs::write(&target, "new").unwrap();
    drop(journal);
    assert_eq!(
        fixture.recover(&mut Files::default()).unwrap(),
        Outcome::Undone
    );
    assert_eq!(bytes(&target).unwrap().as_deref(), Some("before"));
}

#[test]
fn a_sealed_applied_transaction_recovers_forward_even_without_the_last_done_mark() {
    let fixture = Fixture::new();
    let target = fixture.target();
    let manifest = fixture.manifest();
    let mut journal = fixture.journal();
    let index = journal.prepare(step(&target, None, "new")).unwrap();
    fs::write(target, "new").unwrap();
    journal.done(index).unwrap();
    let mut final_step = step(&manifest, None, "owned");
    final_step.action = Action::Inventory {
        file: final_step.action.file().unwrap().clone(),
        contents: "owned".into(),
        mode: 0o600,
    };
    journal.prepare(final_step).unwrap();
    fs::write(&manifest, "owned").unwrap();
    drop(journal);
    assert_eq!(
        fixture.recover(&mut Files::default()).unwrap(),
        Outcome::Committed
    );
    assert_eq!(bytes(&manifest).unwrap().as_deref(), Some("owned"));
}

#[test]
fn undo_resumes_between_restoring_a_target_and_recording_its_restoration() {
    let fixture = Fixture::new();
    let target = fixture.target();
    let mut journal = fixture.journal();
    for (before, after) in [
        (None, "first"),
        (Some("first"), "second"),
        (Some("second"), "third"),
    ] {
        let index = journal.prepare(step(&target, before, after)).unwrap();
        // Interrupt after writing ahead the third revision, not after applying it.
        if after != "third" {
            fs::write(&target, after).unwrap();
            journal.done(index).unwrap();
        }
    }
    drop(journal);
    let mut backend = Files {
        interrupt_after_undo: Some(target.clone()),
    };
    assert_eq!(
        fixture.recover(&mut backend).unwrap_err(),
        "interrupted after restoring bytes and before recording undo"
    );
    assert_eq!(bytes(&target).unwrap().as_deref(), Some("first"));
    assert_eq!(
        fixture.recover(&mut Files::default()).unwrap(),
        Outcome::Undone
    );
    assert_eq!(bytes(&target).unwrap(), None);
}

#[test]
fn a_sealed_write_chain_checks_the_final_bytes_not_earlier_revisions() {
    let fixture = Fixture::new();
    let target = fixture.target();
    let manifest = fixture.manifest();
    let mut journal = fixture.journal();
    for (before, after) in [
        (None, "first"),
        (Some("first"), "second"),
        (Some("second"), "third"),
    ] {
        let index = journal.prepare(step(&target, before, after)).unwrap();
        fs::write(&target, after).unwrap();
        journal.done(index).unwrap();
    }
    let mut final_step = step(&manifest, None, "owned");
    final_step.action = Action::Inventory {
        file: final_step.action.file().unwrap().clone(),
        contents: "owned".into(),
        mode: 0o600,
    };
    journal.prepare(final_step).unwrap();
    fs::write(&manifest, "owned").unwrap();
    drop(journal);
    assert_eq!(
        fixture.recover(&mut Files::default()).unwrap(),
        Outcome::Committed
    );
    assert_eq!(bytes(&target).unwrap().as_deref(), Some("third"));
}

#[test]
fn a_torn_handover_finalize_entry_does_not_turn_the_handover_into_an_undo() {
    let fixture = Fixture::new();
    let target = fixture.target();
    let manifest = fixture.manifest();
    let mut journal = fixture.journal();
    let mut placement = step(&target, None, "new");
    placement.action = Action::Bootstrap {
        file: placement.action.file().unwrap().clone(),
        archive_sha256: "d".repeat(64),
        version: "1.2.3".into(),
    };
    journal.prepare(placement).unwrap();
    fs::write(&target, "new").unwrap();
    drop(journal);
    assert!(Journal::open(&manifest)
        .unwrap()
        .unwrap()
        .handover()
        .is_some());
    append_torn(&manifest);
    assert!(Journal::open(&manifest)
        .unwrap()
        .unwrap()
        .handover()
        .is_some());
}

#[test]
fn a_broken_chain_is_refused_before_it_can_act() {
    let fixture = Fixture::new();
    let target = fixture.target();
    let mut journal = fixture.journal();
    journal.prepare(step(&target, None, "first")).unwrap();
    let error = journal.prepare(step(&target, None, "second")).unwrap_err();
    assert!(error.contains("state chain"), "{error}");
    assert!(error.contains(&target.display().to_string()), "{error}");
    assert_eq!(
        Journal::open(&fixture.manifest())
            .unwrap()
            .unwrap()
            .next_index(),
        1
    );
}
