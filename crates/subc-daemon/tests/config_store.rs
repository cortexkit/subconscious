#![cfg(feature = "test-support")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use subc_daemon::config_store::{StoreRoot, STORE_FILE};

struct Rig(cortexkit_test_support::ScratchDir);
impl Rig {
    fn new() -> Self {
        Self(cortexkit_test_support::ScratchDir::new(
            "config-store-child",
        ))
    }
    fn command(&self, mode: &str, sealed: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ckdev-config-store-fixture"));
        command
            .arg(mode)
            .arg(self.0.path())
            .arg(if sealed { "sealed" } else { "digest" });
        // These decoys must not redirect any part of an explicitly isolated rig.
        for key in [
            "HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
            "XDG_CONFIG_HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "USERPROFILE",
        ] {
            command.env(key, self.0.join("decoy"));
        }
        command.env("SUBC_CONNECTION_FILE", self.0.join("decoy-connection"));
        command
    }
    fn run(&self, mode: &str, sealed: bool) -> String {
        let output = self.command(mode, sealed).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }
    fn crash_at(&self, point: &str, sealed: bool) {
        let reached = self.0.join("pause.reached");
        let _ = fs::remove_file(&reached);
        let mut child = self
            .command(point, sealed)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !reached.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("pause child exited: {status}");
            }
            if started.elapsed() > Duration::from_secs(10) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("child never reached {point}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[test]
fn config_store_child_crash_boundaries_and_sealed_anchor_catchup() {
    for sealed in [false, true] {
        let rig = Rig::new();
        rig.run("stamp", sealed);
        let previous = fs::read(rig.0.join(STORE_FILE)).unwrap();
        rig.crash_at("before-rename", sealed);
        assert_eq!(fs::read(rig.0.join(STORE_FILE)).unwrap(), previous);
        assert!(rig.run("status", sealed).contains("state: Stamped"));
        rig.crash_at("after-rename", sealed);
        let next = fs::read(rig.0.join(STORE_FILE)).unwrap();
        assert_ne!(next, previous);
        assert!(rig.run("status", sealed).contains("state: Stamped"));
        if sealed {
            rig.crash_at("before-anchor", true);
            assert!(rig.run("status", true).contains("state: Stamped"));
            assert!(rig.run("status", true).contains("anchor_pending: false"));
        }
    }
}

#[test]
fn config_store_file_key_failure_retry_and_access_denial() {
    let rig = Rig::new();
    assert!(rig
        .run("fail-anchor", true)
        .contains("anchor_pending: false"));
    assert!(rig.run("deny-key", true).contains("state: StoreUnverified"));
    fs::remove_file(rig.0.join("test-key")).unwrap();
    assert!(rig.run("status", true).contains("state: StoreUnverified"));
}

#[test]
fn config_store_spawn_and_swap_pause_hooks_are_reachable() {
    let rig = Rig::new();
    rig.crash_at("spawn-pause", false);
    rig.crash_at("swap-pause", false);
    assert!(!rig.0.join(STORE_FILE).exists());
}

#[test]
fn config_store_root_and_all_hooks_reject_non_ckdev_binary() {
    let rig = Rig::new();
    let source = Path::new(env!("CARGO_BIN_EXE_ckdev-config-store-fixture"));
    let destination = rig.0.join(if cfg!(windows) {
        "ordinary-store.exe"
    } else {
        "ordinary-store"
    });
    fs::copy(source, &destination).unwrap();
    let output = Command::new(destination)
        .arg("stamp")
        .arg(rig.0.path())
        .arg("digest")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "hooks-denied"
    );
    assert!(!rig.0.join(STORE_FILE).exists());
    assert!(StoreRoot::from_service_definition(PathBuf::from("relative")).is_err());
}

#[cfg(windows)]
#[test]
fn config_store_windows_default_is_profile_local_not_roaming() {
    let expected = subc_os::profile_dir()
        .unwrap()
        .join("AppData")
        .join("Local")
        .join("cortexkit")
        .join("config");
    assert_eq!(StoreRoot::installation_default().unwrap().path(), expected);
    let rig = Rig::new();
    assert_eq!(Path::new(rig.run("windows-root", false).trim()), expected);
    assert!(!expected.components().any(|c| c.as_os_str() == "Roaming"));
}
