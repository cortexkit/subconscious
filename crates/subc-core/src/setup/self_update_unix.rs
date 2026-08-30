use std::{
    fs,
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use super::{upgrade_assets::sha256_file, upgrade_verification::destination_inode};

/// Stage the already-verified candidate beside its destination, so the final
/// rename is within one filesystem and therefore atomically replaces `ck`.
pub(super) fn replace_verified_candidate(
    destination: &Path,
    candidate: &Path,
) -> Result<(String, Option<PathBuf>), String> {
    use std::os::unix::fs::PermissionsExt;

    let temporary = unique_sibling(destination)?;
    let prior_inode = destination_inode(destination)?;
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
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755)).map_err(|error| {
            format!(
                "refusal: could not mark staged ck replacement {} executable: {error}",
                temporary.display()
            )
        })?;
        fs::rename(&temporary, destination).map_err(|error| {
            format!(
                "refusal: could not atomically rename verified ck replacement {} over {}: {error}",
                temporary.display(),
                destination.display()
            )
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;

    let replacement_inode = destination_inode(destination)?;
    Ok((
        format!(
            "Unix self-update atomically renamed the verified replacement; running process retains prior inode={prior_inode}; replacement destination inode={replacement_inode}"
        ),
        None,
    ))
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
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        process::{self, Command},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use serde_json::Map;

    use super::*;
    use crate::setup::{inventory::Inventory, self_update};

    const TEST_NAME: &str =
        "setup::self_update_unix::tests::unix_self_update_keeps_running_process_on_original_inode";
    const TEST_MODE: &str = "CK_SELF_UPDATE_TEST_MODE";

    fn fixture_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "ck-self-update-unix-{name}-{}-{nonce}",
            process::id()
        ))
    }

    fn executable(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("mark executable");
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
        let destination = PathBuf::from(
            env::var("CK_SELF_UPDATE_TEST_DESTINATION").expect("running destination path"),
        );
        // The helper is launched from `destination` before its parent replaces
        // that name. Holding the original file descriptor proves the still-live
        // process retains the prior inode after the destination name moves on.
        let running_image = fs::File::open(&destination).expect("running executable handle");
        let original_inode = running_image
            .metadata()
            .expect("running executable metadata")
            .ino();
        match mode.as_str() {
            "hold" => {
                fs::write(&ready, original_inode.to_string()).expect("ready evidence");
                wait_for(&release);
                let retained_inode = running_image
                    .metadata()
                    .expect("retained executable metadata")
                    .ino();
                fs::write(&result, format!("{original_inode}:{retained_inode}"))
                    .expect("running inode evidence");
            }
            "probe" => {
                fs::write(&result, original_inode.to_string()).expect("probe evidence");
            }
            _ => panic!("unknown self-update helper mode: {mode}"),
        }
        true
    }

    #[test]
    fn unix_self_update_keeps_running_process_on_original_inode() {
        if run_helper_if_requested() {
            return;
        }

        let root = fixture_dir("running-inode");
        fs::create_dir_all(&root).expect("fixture directory");
        let destination = root.join("ck");
        let candidate = root.join("candidate");
        let manifest = root.join("installer-manifest.json");
        let ready = root.join("ready");
        let release = root.join("release");
        let held_result = root.join("held-result");
        let probe_result = root.join("probe-result");
        let test_binary = env::current_exe().expect("test executable");
        fs::copy(&test_binary, &destination).expect("copy installed ck");
        fs::copy(&test_binary, &candidate).expect("copy replacement ck");
        executable(&destination);
        executable(&candidate);
        let prior_inode = destination_inode(&destination).expect("prior inode");

        let mut inventory = Inventory::load(&manifest, "linux-x64").expect("inventory");
        inventory.record("binary-placement", &destination, Map::new());
        inventory.save().expect("save inventory");

        let mut holder = Command::new(&destination)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(TEST_MODE, "hold")
            .env("CK_SELF_UPDATE_TEST_DESTINATION", &destination)
            .env("CK_SELF_UPDATE_TEST_READY", &ready)
            .env("CK_SELF_UPDATE_TEST_RELEASE", &release)
            .env("CK_SELF_UPDATE_TEST_RESULT", &held_result)
            .spawn()
            .expect("start original ck");
        wait_for(&ready);
        assert_eq!(
            fs::read_to_string(&ready).expect("ready inode").trim(),
            prior_inode
        );

        self_update::replace_verified_candidate(&destination, &candidate, &mut inventory)
            .expect("atomic self-update");
        let replacement_inode = destination_inode(&destination).expect("replacement inode");
        assert_ne!(replacement_inode, prior_inode);

        fs::write(&release, "continue").expect("release original ck");
        assert!(holder.wait().expect("holder exit").success());
        assert_eq!(
            fs::read_to_string(&held_result)
                .expect("holder evidence")
                .trim(),
            format!("{prior_inode}:{prior_inode}")
        );

        let probe = Command::new(&destination)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(TEST_MODE, "probe")
            .env("CK_SELF_UPDATE_TEST_DESTINATION", &destination)
            .env("CK_SELF_UPDATE_TEST_READY", &ready)
            .env("CK_SELF_UPDATE_TEST_RELEASE", &release)
            .env("CK_SELF_UPDATE_TEST_RESULT", &probe_result)
            .status()
            .expect("start replacement ck");
        assert!(probe.success());
        assert_eq!(
            fs::read_to_string(&probe_result)
                .expect("probe inode")
                .trim(),
            replacement_inode
        );
        fs::remove_dir_all(root).expect("cleanup");
    }
}
