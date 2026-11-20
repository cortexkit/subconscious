use super::*;

use super::super::files::{self as disk, Content, Kind};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Disk {
    pause: Option<(PathBuf, &'static str)>,
}

impl Recovery for Disk {
    fn is_applied(&mut self, step: &Step) -> Result<bool, String> {
        disk::verify(step)
    }

    fn can_replay(&mut self, id: &str, step: &Step) -> Result<bool, String> {
        disk::can_replay(id, step)
    }

    fn replay(&mut self, id: &str, step: &Step) -> Result<(), String> {
        disk::replay(id, step)
    }

    fn undo(&mut self, id: &str, phase: Undo, step: &Step) -> Result<(), String> {
        if phase == Undo::Restore {
            disk::restore(id, step)?;
            if let Some((path, "undo")) = &self.pause {
                wait_for_kill(path);
            }
        }
        Ok(())
    }

    fn prepare_discard(
        &mut self,
        id: &str,
        step: &Step,
        _outcome: Outcome,
    ) -> Result<Option<String>, String> {
        disk::prepare_discard(id, step)
    }

    fn discard(
        &mut self,
        id: &str,
        step: &Step,
        path: &Path,
        identity: &str,
    ) -> Result<Vec<PathBuf>, crate::setup::install_journal::CleanupRefusal> {
        disk::discard_with(id, step, path, identity, || {
            if let Some((root, "rmdir")) = &self.pause {
                fs::write(root.join("stage"), path.to_str().unwrap()).unwrap();
                wait_for_kill(root);
            }
        })
    }
}

fn wait_for_kill(root: &Path) -> ! {
    fs::write(root.join("at-crash-point"), "ready").unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
#[ignore = "child process for the install-journal crash tests"]
fn crash_child() {
    let root = PathBuf::from(std::env::var_os("CK_TEST_JOURNAL_ROOT").unwrap());
    let point = std::env::var("CK_TEST_JOURNAL_CRASH").unwrap();
    let manifest = root.join("installer-manifest.json");
    let target = root.join("daemon.conf");
    let _lock = super::super::super::install_lock::InstallHold::take(
        &manifest,
        "crash child",
        Duration::ZERO,
    )
    .unwrap();
    let mut journal = Journal::create(&manifest).unwrap();
    #[cfg(windows)]
    if point.starts_with("windows-gap") {
        let target = super::super::bootstrap::destination(&root);
        let candidate = Content::Bytes(b"new ck", 0o755);
        let index = disk::prepare(&mut journal, &target, Kind::File, &candidate).unwrap();
        if point == "windows-gap-sealed" {
            disk::prepare(
                &mut journal,
                &manifest,
                Kind::Inventory,
                &Content::Bytes(b"new ownership", 0o600),
            )
            .unwrap();
        }
        disk::act(&journal, index, candidate).unwrap();
        unreachable!("did not reach the between-renames crash point");
    }
    let candidate = root.join("candidate");
    fs::write(&candidate, b"new config").unwrap();
    let index = disk::prepare(
        &mut journal,
        &target,
        Kind::File,
        &Content::File(&candidate),
    )
    .unwrap();
    if point == "write-ahead" {
        wait_for_kill(&root);
    }
    if matches!(point.as_str(), "stage-mkdir" | "stage-marker") {
        let stage = &journal.step(index).action.file().unwrap().staging_directory;
        subc_os::create_private_directory_exclusive(stage).unwrap();
        if point == "stage-marker" {
            fs::write(stage.join("transaction"), &journal.id().as_bytes()[..3]).unwrap();
        }
        fs::write(root.join("stage"), stage.to_str().unwrap()).unwrap();
        wait_for_kill(&root);
    }
    disk::act(&journal, index, Content::File(&candidate)).unwrap();
    if point == "act" {
        wait_for_kill(&root);
    }
    journal.done(index).unwrap();
    if point == "undo" {
        journal
            .recover(&mut Disk {
                pause: Some((root, "undo")),
            })
            .unwrap();
        unreachable!("did not reach undo crash point");
    }
    let data = Content::Bytes(br#"{"mutations":[]}"#, 0o600);
    match point.as_str() {
        "seal" => {
            disk::prepare(&mut journal, &manifest, Kind::Inventory, &data).unwrap();
            wait_for_kill(&root);
        }
        "rmdir" => {
            disk::place(&mut journal, &manifest, Kind::Inventory, data).unwrap();
            journal
                .commit(&mut Disk {
                    pause: Some((root, "rmdir")),
                })
                .unwrap();
            unreachable!("did not reach rmdir crash point");
        }
        other => panic!("unknown crash point {other}"),
    }
}

fn kill_at(point: &str) -> Fixture {
    let fixture = Fixture::new();
    let root = fs::canonicalize(fixture.directory.path()).unwrap();
    fs::write(fixture.target(), b"old config").unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    #[cfg(windows)]
    if point.starts_with("windows-gap") {
        let target = super::super::bootstrap::destination(&root);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, b"old ck").unwrap();
        command
            .env("CK_TEST_INSTALL_CRASH_POINT", "windows-replacement-gap")
            .env("CK_TEST_INSTALL_CRASH_MARKER", root.join("at-crash-point"));
    }
    let mut child = command
        .args([
            "--exact",
            "setup::install_journal::tests::crash::crash_child",
            "--ignored",
        ])
        .env("CK_TEST_JOURNAL_ROOT", &root)
        .env("CK_TEST_JOURNAL_CRASH", point)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("at-crash-point").exists() && Instant::now() < deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child failed before reaching {point}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(
        root.join("at-crash-point").exists(),
        "child did not reach {point}"
    );
    fixture
}

#[cfg(windows)]
#[test]
fn verified_helper_restores_an_unsealed_gap_or_finishes_a_sealed_gap() {
    for (point, expected) in [
        ("windows-gap", b"old ck"),
        ("windows-gap-sealed", b"new ck"),
    ] {
        let fixture = kill_at(point);
        let root = fs::canonicalize(fixture.directory.path()).unwrap();
        let target = super::super::bootstrap::destination(&root);
        let manifest = root.join("installer-manifest.json");
        assert!(!target.exists());
        let _hold = super::super::super::install_lock::InstallHold::take(
            &manifest,
            "fixture helper",
            Duration::ZERO,
        )
        .unwrap();
        super::super::bootstrap::recover_locked(
            &manifest,
            &mut super::super::bootstrap::FileRecovery,
            true,
        )
        .unwrap();
        assert_eq!(&fs::read(target).unwrap(), expected);
        assert_eq!(manifest.exists(), point.ends_with("sealed"));
        assert!(!Journal::exists(&manifest).unwrap());
    }
}

#[cfg(windows)]
#[test]
fn verified_helper_places_the_requested_release_after_an_ordinary_gap() {
    let fixture = kill_at("windows-gap");
    let root = fs::canonicalize(fixture.directory.path()).unwrap();
    let target = super::super::bootstrap::destination(&root);
    let manifest = root.join("installer-manifest.json");
    let source = root.join("verified-candidate");
    fs::write(&source, "requested replacement").unwrap();
    assert!(!target.exists());
    // The interrupted upgrade still owns the old bytes in the inventory.
    let old_digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(b"old ck"));
    fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1, "platform": "windows-x64",
            "mutations": [{"kind":"managed-binary", "path":target, "sha256":old_digest}]
        }))
        .unwrap(),
    )
    .unwrap();
    super::super::bootstrap::place(&root, &source, "1".repeat(64), "2.0.0".into()).unwrap();
    assert_eq!(fs::read_to_string(target).unwrap(), "requested replacement");
    assert!(Journal::open(&manifest)
        .unwrap()
        .unwrap()
        .handover()
        .is_some());
}

fn recover(fixture: &Fixture) -> Result<Outcome, String> {
    let manifest = fixture.manifest();
    let _lock = super::super::super::install_lock::InstallHold::take(
        &manifest,
        "crash recovery",
        Duration::ZERO,
    )
    .unwrap();
    Journal::open(&manifest)
        .unwrap()
        .unwrap()
        .recover(&mut Disk::default())
}

#[test]
fn killed_after_write_ahead_before_file_placement() {
    let fixture = kill_at("write-ahead");
    assert_eq!(recover(&fixture).unwrap(), Outcome::Undone);
    assert_eq!(fs::read(fixture.target()).unwrap(), b"old config");
}

#[test]
fn a_kill_during_stage_marker_creation_removes_only_the_empty_stage() {
    for point in ["stage-mkdir", "stage-marker"] {
        let fixture = kill_at(point);
        let stage =
            PathBuf::from(fs::read_to_string(fixture.directory.path().join("stage")).unwrap());
        assert!(stage.is_dir());
        assert_eq!(recover(&fixture).unwrap(), Outcome::Undone);
        assert_eq!(fs::read(fixture.target()).unwrap(), b"old config");
        assert!(!stage.exists());
    }
}

#[test]
fn a_partial_stage_marker_does_not_authorize_deleting_other_contents() {
    let fixture = kill_at("stage-marker");
    let stage = PathBuf::from(fs::read_to_string(fixture.directory.path().join("stage")).unwrap());
    let unrelated = stage.join("unrelated");
    fs::write(&unrelated, b"do not remove").unwrap();
    assert!(recover(&fixture).is_err());
    assert_eq!(fs::read(unrelated).unwrap(), b"do not remove");
}

#[test]
fn recovery_refusal_names_the_three_file_states_and_safe_next_step() {
    let fixture = kill_at("act");
    let target = fixture.target();
    fs::write(&target, b"user edited after crash").unwrap();
    let error = recover(&fixture).unwrap_err();
    for bytes in [
        &b"old config"[..],
        &b"new config"[..],
        &b"user edited after crash"[..],
    ] {
        let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(bytes));
        assert!(error.contains(&digest), "{error}");
    }
    assert!(error.contains(&target.display().to_string()), "{error}");
    assert!(error.contains("rerun the installer command"), "{error}");
    assert_eq!(fs::read(target).unwrap(), b"user edited after crash");
}

#[test]
fn killed_after_file_placement_before_marking_it_done() {
    let fixture = kill_at("act");
    assert_eq!(fs::read(fixture.target()).unwrap(), b"new config");
    assert_eq!(recover(&fixture).unwrap(), Outcome::Undone);
    assert_eq!(fs::read(fixture.target()).unwrap(), b"old config");
}

#[test]
fn killed_mid_undo_after_restoring_the_previous_file() {
    let fixture = kill_at("undo");
    assert_eq!(fs::read(fixture.target()).unwrap(), b"old config");
    assert_eq!(recover(&fixture).unwrap(), Outcome::Undone);
    assert_eq!(fs::read(fixture.target()).unwrap(), b"old config");
}

#[test]
fn killed_after_the_seal_replays_the_prepared_inventory() {
    let fixture = kill_at("seal");
    assert!(!fixture.manifest().exists());
    assert_eq!(recover(&fixture).unwrap(), Outcome::Committed);
    assert_eq!(
        fs::read(fixture.manifest()).unwrap(),
        br#"{"mutations":[]}"#
    );
    assert_eq!(fs::read(fixture.target()).unwrap(), b"new config");
}

#[test]
fn killed_after_deleting_the_marker_resumes_closed_cleanup() {
    let fixture = kill_at("rmdir");
    let stage = PathBuf::from(fs::read_to_string(fixture.directory.path().join("stage")).unwrap());
    assert!(stage.is_dir());
    assert_eq!(recover(&fixture).unwrap(), Outcome::Committed);
    assert!(!stage.exists());
}

#[test]
fn closed_cleanup_refuses_a_swapped_directory_after_the_marker_was_deleted() {
    let fixture = kill_at("rmdir");
    let stage = PathBuf::from(fs::read_to_string(fixture.directory.path().join("stage")).unwrap());
    fs::rename(&stage, stage.with_extension("original")).unwrap();
    fs::create_dir(&stage).unwrap();
    let error = recover(&fixture).unwrap_err();
    assert!(error.contains("changed identity"), "{error}");
    assert!(stage.exists());
}

#[test]
fn a_closed_journal_with_unknown_stage_contents_is_not_retired() {
    let fixture = Fixture::new();
    let mut journal = fixture.journal();
    disk::place(
        &mut journal,
        &fixture.target(),
        Kind::File,
        Content::Bytes(b"new", 0o600),
    )
    .unwrap();
    let stage = journal
        .step(0)
        .action
        .file()
        .unwrap()
        .staging_directory
        .clone();
    let intrusion = stage.join("unexpected");
    fs::write(&intrusion, b"untouched").unwrap();
    disk::place(
        &mut journal,
        &fixture.manifest(),
        Kind::Inventory,
        Content::Bytes(b"ownership", 0o600),
    )
    .unwrap();
    let error = journal.commit(&mut Disk::default()).unwrap_err();
    assert!(
        error.contains(&format!(
            "installed and recorded, but cleanup of {} was refused",
            intrusion.display()
        )),
        "{error}"
    );
    assert!(
        error.contains("inspect or remove that file, then rerun"),
        "{error}"
    );
    assert!(error.contains("unknown staged file"), "{error}");
    assert_eq!(fs::read(fixture.manifest()).unwrap(), b"ownership");
    assert_eq!(fs::read(intrusion).unwrap(), b"untouched");
    assert!(Journal::exists(&fixture.manifest()).unwrap());
    assert!(!super::super::retired::path(&fixture.manifest()).exists());
}

#[test]
fn removing_a_file_during_an_unsealed_transaction_is_undone() {
    let fixture = Fixture::new();
    let target = fixture.target();
    fs::write(&target, b"old config").unwrap();
    let mut journal = fixture.journal();
    disk::place(&mut journal, &target, Kind::File, Content::Absent).unwrap();
    drop(journal);
    assert!(!target.exists());
    assert_eq!(recover(&fixture).unwrap(), Outcome::Undone);
    assert_eq!(fs::read(target).unwrap(), b"old config");
}

#[cfg(windows)]
#[test]
fn replacing_and_restoring_a_mapped_windows_ck_keeps_its_public_name() {
    replace_and_restore_mapped_ck();
}

#[cfg(windows)]
#[test]
fn legacy_cleanup_leaves_a_still_mapped_released_image_and_retries_later() {
    let fixture = Fixture::new();
    let root = fs::canonicalize(fixture.directory.path()).unwrap();
    let public = super::super::bootstrap::destination(&root);
    fs::create_dir(public.parent().unwrap()).unwrap();
    fs::write(&public, b"currently installed executable").unwrap();
    let old = public.with_extension("exe.old");
    fs::copy(std::env::current_exe().unwrap(), &old).unwrap();
    let manifest = root.join("installer-manifest.json");
    let mut inventory = super::super::bootstrap::read_inventory(&manifest).unwrap();
    inventory.record("managed-binary", &public, serde_json::Map::new());
    inventory.record(
        "self-update-rollback",
        &old,
        serde_json::Map::from_iter([(
            "replaces".into(),
            serde_json::Value::String(public.display().to_string()),
        )]),
    );
    inventory.save().unwrap();
    let _hold = super::super::super::install_lock::InstallHold::take(
        &manifest,
        "test legacy cleanup",
        Duration::ZERO,
    )
    .unwrap();
    let mut child = map_ck(&old, &root);
    let reconciled = super::super::legacy::reconcile(&manifest);
    let kept = old.is_file();
    let _ = child.kill();
    child.wait().unwrap();
    reconciled.unwrap();
    assert!(kept);
    assert!(super::super::bootstrap::read_inventory(&manifest)
        .unwrap()
        .owns_path("self-update-rollback", &old));
    super::super::legacy::reconcile(&manifest).unwrap();
    assert!(!old.exists());
}

#[cfg(unix)]
#[test]
fn replacing_and_restoring_a_running_unix_ck_keeps_both_processes_alive() {
    replace_and_restore_mapped_ck();
}

#[cfg(any(unix, windows))]
fn replace_and_restore_mapped_ck() {
    let fixture = Fixture::new();
    let root = fs::canonicalize(fixture.directory.path()).unwrap();
    let target = root.join("ck.exe");
    let original = std::env::current_exe().unwrap();
    #[cfg(windows)]
    fs::copy(&original, &target).unwrap();
    #[cfg(unix)]
    assert!(Command::new("cp")
        .arg(&original)
        .arg(&target)
        .status()
        .unwrap()
        .success());
    let mut candidate = fs::read(&original).unwrap();
    candidate.extend_from_slice(b"new ck image");
    let mut child = map_ck(&target, &root);
    let mut journal = fixture.journal();
    let result = (|| {
        disk::place(
            &mut journal,
            &target,
            Kind::File,
            Content::Bytes(&candidate, 0o755),
        )?;
        assert_eq!(fs::read(&target).unwrap(), candidate);
        let new_root = root.join("candidate-child");
        fs::create_dir(&new_root).unwrap();
        let mut new_child = map_ck(&target, &new_root);
        let restored = disk::restore(journal.id(), journal.step(0));
        let both_running =
            child.try_wait().unwrap().is_none() && new_child.try_wait().unwrap().is_none();
        let _ = new_child.kill();
        new_child.wait().unwrap();
        restored?;
        assert!(both_running);
        assert_eq!(disk::digest(&target)?, disk::digest(&original)?);
        Ok::<_, String>(())
    })();
    let _ = child.kill();
    child.wait().unwrap();
    result.unwrap();
}

#[cfg(any(unix, windows))]
fn map_ck(target: &Path, root: &Path) -> std::process::Child {
    let mut child = Command::new(target)
        .args([
            "--exact",
            "setup::install_journal::tests::crash::mapped_ck_child",
            "--ignored",
        ])
        .env("CK_TEST_JOURNAL_ROOT", root)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let marker = root.join("at-crash-point");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline && child.try_wait().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "ck.exe was not mapped by the scratch child"
    );
    child
}

#[cfg(any(unix, windows))]
#[test]
#[ignore = "scratch ck.exe mapped by the parent test"]
fn mapped_ck_child() {
    wait_for_kill(&PathBuf::from(
        std::env::var_os("CK_TEST_JOURNAL_ROOT").unwrap(),
    ));
}
