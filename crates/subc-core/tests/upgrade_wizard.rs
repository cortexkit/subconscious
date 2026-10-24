//! The terminal app's update wizard and `ck upgrade --check` refuse with the
//! same words, because both take them from the same function. This runs the
//! wizard's check in this process and `ck upgrade --check` as a child, both
//! against one isolated installation, and compares the two.
//!
//! The wizard's check reads the installation from the environment, as `ck`
//! does, so this file holds a single test: setting the environment in a
//! process where other tests run at the same time would race them.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;

use ck_ui::upgrade::{UpgradeEvent, UpgradeService};
use cortexkit_test_support::ScratchDir;
use subc_core::setup::wizard::{SetupUpgradeService, UpgradeDaemon};

/// A daemon nobody may ask: each refusal here comes before the plan needs
/// one.
struct NoDaemon;

impl UpgradeDaemon for NoDaemon {
    fn connection_path(&self) -> Option<PathBuf> {
        panic!("a refused upgrade reads no connection file")
    }

    fn supervised_modules(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<BTreeSet<String>, String>> + Send + '_>> {
        panic!("a refused upgrade asks the daemon nothing")
    }
}

/// The environment `ck` and the wizard both read: every user folder under
/// `root`, and a release index nobody serves, which a refusal never reaches.
fn environment(root: &Path) -> Vec<(&'static str, PathBuf)> {
    let home = root.join("home");
    vec![
        ("HOME", home.clone()),
        ("USERPROFILE", home.clone()),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("LOCALAPPDATA", home.join(".local/share")),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_RUNTIME_DIR", root.join("runtime")),
        ("TMPDIR", root.join("tmp")),
        (
            "CK_UPDATE_CACHE_PATH",
            root.join("cache/update-metadata.json"),
        ),
        (
            "CK_RELEASE_INDEX_URL",
            PathBuf::from("http://127.0.0.1:9/index.json"),
        ),
        ("SUBC_CONNECTION_FILE", home.join("no-daemon.json")),
    ]
}

#[test]
fn the_wizard_refuses_with_the_text_ck_upgrade_check_prints() {
    let ck = PathBuf::from(env!("CARGO_BIN_EXE_ck-under-test"));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // Nothing installed by ck setup, then a CortexKit program in the managed
    // folder that ck setup didn't install.
    for unowned in [false, true] {
        let root = ScratchDir::new(if unowned {
            "wizard-refusal-unowned"
        } else {
            "wizard-refusal-nothing-managed"
        });
        let env = environment(root.path());
        for (_, path) in &env {
            if !path.to_string_lossy().starts_with("http") {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            }
        }
        let bin = root.path().join("home/.local/share/cortexkit/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let aft = bin.join("ck-aft");
        if unowned {
            std::fs::write(&aft, "not run: an unowned program is never executed\n").unwrap();
        }
        for (name, value) in &env {
            std::env::set_var(name, value);
        }

        let output = Command::new(&ck)
            .args(["upgrade", "--check"])
            .envs(env.iter().map(|(k, v)| (*k, v.as_os_str())))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stderr.is_empty(), "{output:?}");
        let printed = String::from_utf8(output.stdout).unwrap();

        let service = SetupUpgradeService::new(&ck, Box::new(NoDaemon), runtime.handle().clone());
        let check = service.check().expect("a refusal is a plan, not an error");
        let refusal = check.refusal.clone().expect("the wizard refuses too");
        assert_eq!(printed, format!("{refusal}\n"));
        assert!(check.rows.is_empty() && check.stages.is_empty());
        if unowned {
            assert!(
                refusal.contains("~/.local/share/cortexkit/bin/ck-aft"),
                "{refusal}"
            );
        } else {
            assert!(
                refusal.starts_with("No CortexKit programs installed by ck setup"),
                "{refusal}"
            );
        }

        // Running refuses the same way and changes nothing.
        let mut events = Vec::new();
        let outcome = service.run(&mut |event| events.push(event), &mut |question| {
            panic!("a refused upgrade asks no rollback question: {question:?}")
        });
        assert_eq!(events, vec![UpgradeEvent::Planned(check)]);
        assert_eq!(outcome.error.as_deref(), Some(refusal.as_str()));
        assert!(outcome.installed.is_empty());
        assert!(!root
            .path()
            .join("home/.local/share/cortexkit/installer-manifest.json")
            .exists());
        if unowned {
            assert_eq!(
                std::fs::read_to_string(&aft).unwrap(),
                "not run: an unowned program is never executed\n"
            );
        }
    }
}
