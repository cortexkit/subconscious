use std::{
    fs,
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

use super::{
    inventory::Inventory, upgrade_assets::sha256_file, upgrade_verification::destination_inode,
};

/// Windows cannot replace the image at its current name. Keep the previous
/// executable under an inventory-owned rollback name until a later invocation
/// has started successfully and removes it.
pub(super) fn replace_verified_candidate(
    destination: &Path,
    candidate: &Path,
    inventory: &mut Inventory,
) -> Result<(String, Option<PathBuf>), String> {
    let temporary = unique_sibling(destination)?;
    let previous = destination.with_extension("exe.old");
    if previous.exists() {
        return Err(format!(
            "refusal: prior Windows self-update evidence already exists at {}; refusing to overwrite rollback evidence",
            previous.display()
        ));
    }

    let candidate_digest = sha256_file(candidate)?;
    let result = (|| {
        fs::copy(candidate, &temporary).map_err(|error| {
            format!(
                "refusal: could not stage verified ck replacement at {}: {error}",
                temporary.display()
            )
        })?;
        if sha256_file(&temporary)? != candidate_digest {
            return Err(format!(
                "refusal: staged ck replacement at {} no longer matches the verified candidate",
                temporary.display()
            ));
        }
        fs::rename(destination, &previous).map_err(|error| {
            format!(
                "refusal: could not rename running ck executable {} to {} without elevation: {error}",
                destination.display(),
                previous.display()
            )
        })?;
        let mut rollback_fields = Map::new();
        rollback_fields.insert(
            "replaces".to_string(),
            Value::String(destination.to_string_lossy().into_owned()),
        );
        inventory.record("self-update-rollback", &previous, rollback_fields);
        inventory.save().map_err(|error| {
            format!(
                "refusal: ck.exe was retained at {} as rollback evidence but installer-manifest.json could not record it: {error}",
                previous.display()
            )
        })?;
        fs::rename(&temporary, destination).map_err(|error| {
            format!(
                "refusal: ck.exe was retained at {} as rollback evidence but the verified replacement could not be installed at {}: {error}",
                previous.display(),
                destination.display()
            )
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;

    Ok((
        format!(
            "Windows self-update renamed the running executable to {}; replacement destination inode={}",
            previous.display(),
            destination_inode(destination)?
        ),
        Some(previous),
    ))
}

/// Delete only rollback evidence that the installer manifest establishes as a
/// self-update artifact. A user-created `ck.exe.old` must remain untouched.
pub(super) fn cleanup_owned_previous(
    executable: &Path,
    inventory: &mut Inventory,
) -> Result<(), String> {
    let previous = executable.with_extension("exe.old");
    if !previous.exists() {
        return Ok(());
    }
    if !inventory.owns_path("self-update-rollback", &previous) {
        return Err(format!(
            "refusal: {} exists but is not installer-manifest.json-owned self-update rollback evidence; refusing to remove it",
            previous.display()
        ));
    }

    fs::remove_file(&previous).map_err(|error| {
        format!(
            "could not delete prior Windows self-update executable {}: {error}",
            previous.display()
        )
    })?;
    inventory.remove_owned_path("self-update-rollback", &previous);
    inventory.save().map_err(|error| {
        format!(
            "could not reconcile installer-manifest.json after deleting {}: {error}",
            previous.display()
        )
    })
}

fn unique_sibling(destination: &Path) -> Result<PathBuf, String> {
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "refusal: managed ck destination {} has no parent directory",
            destination.display()
        )
    })?;
    let name = destination
        .file_name()
        .ok_or_else(|| {
            format!(
                "refusal: managed ck destination {} has no file name",
                destination.display()
            )
        })?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("refusal: clock before Unix epoch: {error}"))?
        .as_nanos();
    let temporary = parent.join(format!(".{name}.self-update-{}-{nonce}", process::id()));
    if temporary.exists() {
        return Err(format!(
            "refusal: temporary ck replacement path already exists: {}",
            temporary.display()
        ));
    }
    Ok(temporary)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    use serde_json::Map;

    use super::*;
    use subc_core::test_support::TestTempDir;

    const TEST_NAME: &str =
        "setup::self_update_windows::tests::replacement_preserves_old_until_a_successful_next_invocation_cleans_it";
    const TEST_MODE: &str = "CK_SELF_UPDATE_WINDOWS_TEST_MODE";

    fn fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn run_helper_if_requested() -> bool {
        let Ok(mode) = env::var(TEST_MODE) else {
            return false;
        };
        let ready = PathBuf::from(env::var("CK_SELF_UPDATE_TEST_READY").expect("ready path"));
        let release = PathBuf::from(env::var("CK_SELF_UPDATE_TEST_RELEASE").expect("release path"));
        let result = PathBuf::from(env::var("CK_SELF_UPDATE_TEST_RESULT").expect("result path"));
        match mode.as_str() {
            "hold" => {
                fs::write(&ready, "running").expect("ready evidence");
                wait_for(&release);
                fs::write(&result, "original process remained active")
                    .expect("running process evidence");
            }
            "probe" => {
                fs::write(&result, "replacement started").expect("replacement startup evidence");
            }
            _ => panic!("unknown self-update helper mode: {mode}"),
        }
        true
    }

    #[test]
    fn replacement_preserves_old_until_a_successful_next_invocation_cleans_it() {
        if run_helper_if_requested() {
            return;
        }

        let root = fixture_dir("rename-replace-cleanup");
        let destination = root.join("ck.exe");
        let candidate = root.join("candidate.exe");
        let manifest = root.join("installer-manifest.json");
        let ready = root.join("ready");
        let release = root.join("release");
        let held_result = root.join("held-result");
        let probe_result = root.join("probe-result");
        let test_binary = env::current_exe().expect("test executable");
        fs::copy(&test_binary, &destination).expect("copy installed ck");
        fs::copy(&test_binary, &candidate).expect("copy replacement ck");

        let mut inventory = Inventory::load(&manifest, "windows-x64").expect("inventory");
        inventory.record("binary-placement", &destination, Map::new());
        inventory.save().expect("save inventory");

        let mut holder = Command::new(&destination)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(TEST_MODE, "hold")
            .env("CK_SELF_UPDATE_TEST_READY", &ready)
            .env("CK_SELF_UPDATE_TEST_RELEASE", &release)
            .env("CK_SELF_UPDATE_TEST_RESULT", &held_result)
            .spawn()
            .expect("start original ck");
        wait_for(&ready);

        super::super::self_update::replace_verified_candidate(
            &destination,
            &candidate,
            &"aa".repeat(32),
            &mut inventory,
        )
        .expect("rename and replace while original ck runs");
        let previous = destination.with_extension("exe.old");
        assert!(
            previous.exists(),
            "rollback evidence remains until next startup"
        );
        assert!(destination.exists(), "replacement is installed as ck.exe");

        fs::write(&release, "continue").expect("release original ck");
        assert!(holder.wait().expect("holder exit").success());
        assert_eq!(
            fs::read_to_string(&held_result).expect("holder evidence"),
            "original process remained active"
        );

        let probe = Command::new(&destination)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(TEST_MODE, "probe")
            .env("CK_SELF_UPDATE_TEST_READY", &ready)
            .env("CK_SELF_UPDATE_TEST_RELEASE", &release)
            .env("CK_SELF_UPDATE_TEST_RESULT", &probe_result)
            .status()
            .expect("start replacement ck");
        assert!(probe.success());
        assert_eq!(
            fs::read_to_string(&probe_result).expect("probe evidence"),
            "replacement started"
        );

        // Rust's test harness bypasses ck's normal post-command cleanup. Calling
        // the same helper after the probe models the successful next invocation.
        cleanup_owned_previous(&destination, &mut inventory).expect("successful next invocation");
        assert!(!previous.exists());
        let inventory = Inventory::load(&manifest, "windows-x64").expect("reloaded inventory");
        assert!(!inventory.owns_path("self-update-rollback", &previous));
    }

    #[test]
    fn cleanup_refuses_to_delete_an_unowned_old_executable() {
        let root = fixture_dir("unowned-old");
        let destination = root.join("ck.exe");
        let previous = destination.with_extension("exe.old");
        let manifest = root.join("installer-manifest.json");
        fs::write(&previous, "user evidence").expect("old executable");
        let mut inventory = Inventory::load(&manifest, "windows-x64").expect("inventory");

        let error = cleanup_owned_previous(&destination, &mut inventory)
            .expect_err("unowned rollback evidence must be preserved");

        assert!(error.contains("not installer-manifest.json-owned"));
        assert_eq!(
            fs::read_to_string(&previous).expect("old bytes"),
            "user evidence"
        );
    }
}
