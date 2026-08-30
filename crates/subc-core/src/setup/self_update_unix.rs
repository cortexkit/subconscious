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
        io::{BufRead, BufReader, Write},
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        process::{self, Command, Stdio},
        sync::mpsc::{self, Receiver},
        thread,
        time::{Duration, Instant},
    };

    use serde_json::Map;

    use super::*;
    use crate::setup::{inventory::Inventory, self_update};
    use subc_core::test_support::TestTempDir;

    const TEST_NAME: &str =
        "setup::self_update_unix::tests::unix_self_update_keeps_running_process_on_original_inode";
    const TEST_MODE: &str = "CK_SELF_UPDATE_TEST_MODE";
    const HELPER_READY_PREFIX: &str = "CK_SELF_UPDATE_TEST_READY:";
    const HELPER_RESULT_PREFIX: &str = "CK_SELF_UPDATE_TEST_RESULT:";
    const HELPER_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

    fn fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    fn executable(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("mark executable");
    }

    fn wait_for_helper_line(receiver: &Receiver<String>, prefix: &str) -> String {
        let deadline = Instant::now() + HELPER_WAIT_TIMEOUT;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| {
                    panic!("timed out waiting for helper output beginning with {prefix}")
                });
            match receiver.recv_timeout(remaining) {
                Ok(line) => {
                    if let Some(value) = line.strip_prefix(prefix) {
                        return value.to_owned();
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for helper output beginning with {prefix}");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("helper exited before writing output beginning with {prefix}");
                }
            }
        }
    }

    fn wait_for_holder_exit(holder: &mut process::Child) -> process::ExitStatus {
        let deadline = Instant::now() + HELPER_WAIT_TIMEOUT;
        loop {
            match holder.try_wait().expect("check holder exit") {
                Some(status) => return status,
                None => assert!(
                    Instant::now() < deadline,
                    "timed out waiting for holder exit"
                ),
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn run_helper_if_requested() -> bool {
        let Ok(mode) = env::var(TEST_MODE) else {
            return false;
        };
        let destination = PathBuf::from(
            env::var("CK_SELF_UPDATE_TEST_DESTINATION").expect("running destination path"),
        );
        // The helper starts from `destination` before its parent replaces that
        // name. Keeping this handle open proves the live process retains the
        // original inode after the destination is atomically renamed.
        let running_image = fs::File::open(&destination).expect("running executable handle");
        let original_inode = running_image
            .metadata()
            .expect("running executable metadata")
            .ino();
        match mode.as_str() {
            "hold" => {
                println!("{HELPER_READY_PREFIX}{original_inode}");
                std::io::stdout().flush().expect("flush ready evidence");

                let mut release = String::new();
                std::io::stdin()
                    .read_line(&mut release)
                    .expect("read release signal");
                assert_eq!(release.trim(), "release", "unexpected release signal");

                let retained_inode = running_image
                    .metadata()
                    .expect("retained executable metadata")
                    .ino();
                println!("{HELPER_RESULT_PREFIX}{original_inode}:{retained_inode}");
                std::io::stdout()
                    .flush()
                    .expect("flush retained inode evidence");
            }
            "probe" => {
                println!("{HELPER_RESULT_PREFIX}{original_inode}");
                std::io::stdout()
                    .flush()
                    .expect("flush probe inode evidence");
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
        let destination = root.join("ck");
        let candidate = root.join("candidate");
        let manifest = root.join("installer-manifest.json");
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start original ck");
        let holder_stdout = holder.stdout.take().expect("capture holder output");
        let (helper_output_tx, helper_output_rx) = mpsc::channel();
        let output_reader = thread::spawn(move || {
            for line in BufReader::new(holder_stdout).lines() {
                let line = line.expect("read holder output");
                if helper_output_tx.send(line).is_err() {
                    return;
                }
            }
        });
        assert_eq!(
            wait_for_helper_line(&helper_output_rx, HELPER_READY_PREFIX),
            prior_inode.to_string()
        );

        self_update::replace_verified_candidate(&destination, &candidate, &mut inventory)
            .expect("atomic self-update");
        let replacement_inode = destination_inode(&destination).expect("replacement inode");
        assert_ne!(replacement_inode, prior_inode);

        let mut holder_stdin = holder.stdin.take().expect("capture holder input");
        holder_stdin
            .write_all(b"release\n")
            .expect("release original ck");
        holder_stdin.flush().expect("flush release signal");
        drop(holder_stdin);
        assert_eq!(
            wait_for_helper_line(&helper_output_rx, HELPER_RESULT_PREFIX),
            format!("{prior_inode}:{prior_inode}")
        );
        assert!(wait_for_holder_exit(&mut holder).success());
        output_reader.join().expect("finish holder output reader");

        let probe = Command::new(&destination)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(TEST_MODE, "probe")
            .env("CK_SELF_UPDATE_TEST_DESTINATION", &destination)
            .output()
            .expect("start replacement ck");
        assert!(
            probe.status.success(),
            "replacement probe failed: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let probe_output = String::from_utf8(probe.stdout).expect("probe output is UTF-8");
        assert_eq!(
            probe_output
                .lines()
                .find_map(|line| line.strip_prefix(HELPER_RESULT_PREFIX))
                .expect("probe inode evidence"),
            replacement_inode.to_string()
        );
    }
}
