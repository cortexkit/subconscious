use super::super::{Outcome, Recovery, Undo};
use super::*;
use crate::setup::runtime::CommandResult;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Runner;

impl CommandRunner for Runner {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        match program {
            "ditto" => {
                fs::create_dir(&args[1]).unwrap();
                fs::copy(
                    Path::new(&args[0]).join("daemon"),
                    Path::new(&args[1]).join("daemon"),
                )
                .unwrap();
            }
            "/usr/libexec/PlistBuddy" => {
                return Ok(CommandResult {
                    success: true,
                    exit_code: Some(0),
                    stdout: "5R5846NBPW.io.cortexkit.daemon".into(),
                    stderr: String::new(),
                })
            }
            "codesign" | "security" => {}
            other => panic!("unexpected command {other}"),
        }
        Ok(CommandResult {
            success: true,
            exit_code: Some(0),
            ..CommandResult::default()
        })
    }
}

impl Recovery for Runner {
    fn is_applied(&mut self, step: &Step) -> Result<bool, String> {
        is_applied(step, self)
    }

    fn can_replay(&mut self, id: &str, step: &Step) -> Result<bool, String> {
        can_replay(id, step)
    }

    fn replay(&mut self, id: &str, step: &Step) -> Result<(), String> {
        replay(id, step, self)
    }

    fn undo(&mut self, id: &str, phase: Undo, step: &Step) -> Result<(), String> {
        if phase == Undo::Restore {
            undo(id, step, self)?;
        }
        Ok(())
    }

    fn prepare_discard(
        &mut self,
        id: &str,
        step: &Step,
        _outcome: Outcome,
    ) -> Result<Option<String>, String> {
        files::prepare_discard(id, step)
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

pub(super) fn after_exchange() {
    if let Some(root) = std::env::var_os("CK_TEST_BUNDLE_CRASH") {
        fs::write(PathBuf::from(root).join("swapped"), "ready").unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[test]
fn an_interrupted_uninstall_restores_the_entire_bundle() {
    let scratch = cortexkit_test_support::ScratchDir::new("journal-bundle-uninstall");
    let root = fs::canonicalize(scratch.path()).unwrap();
    let manifest = root.join("manifest");
    let bundle = root.join("Subconscious.app");
    fs::create_dir(&bundle).unwrap();
    fs::write(bundle.join("daemon"), "old executable").unwrap();
    fs::write(bundle.join("Info.plist"), "old metadata").unwrap();
    let _lock =
        crate::setup::InstallHold::take(&manifest, "test uninstall", Duration::ZERO).unwrap();
    let mut journal = Journal::create(&manifest).unwrap();
    remove(&mut journal, &bundle, &mut Runner).unwrap();
    assert!(!bundle.exists());
    drop(journal);
    assert_eq!(
        Journal::open(&manifest)
            .unwrap()
            .unwrap()
            .recover(&mut Runner)
            .unwrap(),
        Outcome::Undone
    );
    assert_eq!(
        fs::read_to_string(bundle.join("daemon")).unwrap(),
        "old executable"
    );
    assert_eq!(
        fs::read_to_string(bundle.join("Info.plist")).unwrap(),
        "old metadata"
    );
}

#[test]
#[ignore = "child process for the bundle exchange crash test"]
fn bundle_child() {
    let root = PathBuf::from(std::env::var_os("CK_TEST_BUNDLE_CRASH").unwrap());
    let manifest = root.join("manifest");
    let _lock =
        crate::setup::InstallHold::take(&manifest, "test placement", Duration::ZERO).unwrap();
    let mut journal = Journal::create(&manifest).unwrap();
    place(
        &mut journal,
        &root.join("Subconscious.app"),
        &root.join("candidate.app"),
        &mut Runner,
    )
    .unwrap();
    unreachable!("atomic exchange did not reach its crash point");
}

#[test]
fn killed_after_exchange_restores_the_old_bundle_without_missing_the_public_path() {
    let scratch = cortexkit_test_support::ScratchDir::new("journal-bundle-crash");
    let root = fs::canonicalize(scratch.path()).unwrap();
    let manifest = root.join("manifest");
    let bundle = root.join("Subconscious.app");
    let candidate = root.join("candidate.app");
    fs::create_dir(&bundle).unwrap();
    fs::write(bundle.join("daemon"), "old executable").unwrap();
    fs::create_dir(&candidate).unwrap();
    fs::write(candidate.join("daemon"), "new executable").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "setup::install_journal::bundle::tests::bundle_child",
            "--ignored",
        ])
        .env("CK_TEST_BUNDLE_CRASH", &root)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("swapped").exists() && Instant::now() < deadline {
        assert!(bundle.is_dir(), "the canonical bundle disappeared");
        assert!(
            child.try_wait().unwrap().is_none(),
            "child did not exchange the bundles"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(root.join("swapped").exists());
    assert_eq!(
        fs::read_to_string(bundle.join("daemon")).unwrap(),
        "new executable"
    );
    let _lock =
        crate::setup::InstallHold::take(&manifest, "test recovery", Duration::ZERO).unwrap();
    let journal = Journal::open(&manifest).unwrap().unwrap();
    assert_eq!(journal.recover(&mut Runner).unwrap(), Outcome::Undone);
    assert_eq!(
        fs::read_to_string(bundle.join("daemon")).unwrap(),
        "old executable"
    );
}
