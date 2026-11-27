use super::*;
use crate::setup::{
    install_journal::{system::SystemRecovery, Outcome},
    runtime::{self, RuntimePlatform, SystemCommandRunner},
    InstallHold,
};
use cortexkit_test_support::ScratchDir;
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
#[ignore = "child process for directory placement crash tests"]
fn directory_child() {
    let root = PathBuf::from(std::env::var_os("CK_TEST_DIRECTORY_ROOT").unwrap());
    let manifest = root.join("manifest.json");
    let _lock = InstallHold::take(&manifest, "test placement", Duration::ZERO).unwrap();
    let target = root.join("catalog");
    let mut journal = Journal::create(&manifest).unwrap();
    place(
        &mut journal,
        &target,
        &root.join("candidate"),
        digest(&target).unwrap(),
    )
    .unwrap();
    panic!("child did not stop at the requested crash point");
}

#[test]
fn interrupted_directory_placement_restores_the_whole_previous_tree() {
    for (point, existing, new_visible) in [
        ("directory-staged", true, false),
        ("directory-previous-retained", true, cfg!(unix)),
        ("directory-published", true, true),
        ("directory-done", true, true),
        ("directory-published", false, true),
    ] {
        let scratch = ScratchDir::new("catalog-journal-crash");
        let root = fs::canonicalize(scratch.path()).unwrap();
        let manifest = root.join("manifest.json");
        let target = root.join("catalog");
        let candidate = root.join("candidate");
        let marker = root.join("crash-marker");
        if existing {
            fs::create_dir(&target).unwrap();
            fs::write(target.join("removed.jsonc"), "old").unwrap();
        }
        fs::create_dir(&candidate).unwrap();
        fs::write(candidate.join("replacement.jsonc"), "new").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "setup::install_journal::directory::tests::directory_child",
                "--ignored",
            ])
            .env("CK_TEST_DIRECTORY_ROOT", &root)
            .env("CK_TEST_INSTALL_CRASH_POINT", point)
            .env("CK_TEST_INSTALL_CRASH_MARKER", &marker)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < deadline && child.try_wait().unwrap().is_none() {
            #[cfg(unix)]
            if existing {
                assert!(target.is_dir(), "the exchange exposed a missing catalog");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        assert!(!child.wait().unwrap().success());
        assert_eq!(fs::read_to_string(&marker).as_deref().unwrap(), point);
        assert_eq!(target.join("replacement.jsonc").is_file(), new_visible);
        if new_visible {
            assert!(!target.join("removed.jsonc").exists());
        }
        let _lock = InstallHold::take(&manifest, "test recovery", Duration::ZERO).unwrap();
        let platform = RuntimePlatform::current();
        let mut recovery = SystemRecovery {
            executable: &root.join("no-executable-needed"),
            connection: None,
            platform,
            paths: &runtime::runtime_paths(platform, &root, &root),
            runner: &mut SystemCommandRunner,
        };
        let journal = Journal::open(&manifest).unwrap().unwrap();
        assert_eq!(journal.recover(&mut recovery).unwrap(), Outcome::Undone);
        assert_eq!(target.is_dir(), existing);
        assert!(!target.join("replacement.jsonc").exists());
        if existing {
            assert_eq!(
                fs::read_to_string(target.join("removed.jsonc")).unwrap(),
                "old"
            );
        }
        assert!(Journal::open(&manifest).unwrap().is_none());
    }
}
