#![forbid(unsafe_code)]

//! Process-kill tests of the bootstrap and setup entry points. All install paths
//! are redirected to a scratch root; no service manager or daemon is run.

mod common;

use cortexkit_test_support::ScratchDir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

struct Fixture {
    root: ScratchDir,
    extracted: PathBuf,
    marker: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = ScratchDir::new("ck-journal-cli");
        let temp = root.path().join("temp");
        fs::create_dir(&temp).unwrap();
        let stage = temp.join("verified-download");
        subc_os::create_private_directory_exclusive(&stage).unwrap();
        let extracted = stage.join(binary());
        common::copy_executable(Path::new(env!("CARGO_BIN_EXE_ck")), &extracted);
        let marker = root.path().join("crash-point");
        Self {
            root,
            extracted,
            marker,
        }
    }

    fn command(&self, executable: &Path) -> Command {
        let root = self.root.path();
        let mut command = Command::new(executable);
        command
            .env("HOME", root)
            .env("LOCALAPPDATA", root.join("data"))
            .env("USERPROFILE", root)
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("TMPDIR", root.join("temp"))
            .env("TEMP", root.join("temp"))
            .env("TMP", root.join("temp"))
            .env_remove("CK_BOOTSTRAP_PROFILE")
            .env_remove("CK_BOOTSTRAP_WINDOWS_PATH")
            .env_remove("SUBC_CONNECTION_FILE");
        command
    }

    fn install(&self) -> Command {
        let mut command = self.command(&self.extracted);
        command.args(["__install-bootstrap", &"a".repeat(64), "1.2.3"]);
        command
    }

    fn kill(&self, mut command: Command, point: &str) -> Result<(), String> {
        // Log to a file so a full output pipe cannot stop the child before the marker.
        let log_path = self.root.path().join("crash-child.log");
        let log = fs::File::create(&log_path).unwrap();
        let mut child = command
            .env("CK_TEST_INSTALL_CRASH_POINT", point)
            .env("CK_TEST_INSTALL_CRASH_MARKER", &self.marker)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        // Publication and its disk syncs can be slow, but the hook waits for the kill.
        while !self.marker.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                let log = fs::read(log_path)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_else(|error| format!("could not read child log: {error}"));
                return Err(format!(
                    "child exited before reaching crash point {point:?}: {status}\n{log}"
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success(), "crash point {point:?}");
        let marker = fs::read_to_string(&self.marker)
            .map_err(|error| format!("could not read marker for crash point {point:?}: {error}"))?;
        assert_eq!(marker, point, "crash point {point:?}");
        Ok(())
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("data/cortexkit")
    }
    fn public(&self) -> PathBuf {
        self.data().join("cmd").join(binary())
    }
    fn manifest(&self) -> PathBuf {
        self.data().join("installer-manifest.json")
    }
    fn journal(&self) -> PathBuf {
        self.data().join("installer.journal")
    }
}

fn binary() -> &'static str {
    if cfg!(windows) {
        "ck.exe"
    } else {
        "ck"
    }
}

fn ok(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn bootstrap_exit_before_marker_names_the_crash_point() {
    let fixture = Fixture::new();
    let mut command = fixture.command(&fixture.extracted);
    command.arg("--version");
    let error = fixture.kill(command, "bootstrap-act").unwrap_err();
    assert!(
        error.starts_with("child exited before reaching crash point \"bootstrap-act\":"),
        "{error}"
    );
}

#[test]
fn bootstrap_kill_after_write_ahead_is_recovered_by_the_verified_temporary_ck() {
    let fixture = Fixture::new();
    fixture
        .kill(fixture.install(), "bootstrap-write-ahead")
        .unwrap();
    assert!(fixture.journal().is_file());
    assert!(!fixture.public().exists());
    assert!(!fixture.manifest().exists());
    ok(fixture.install().output().unwrap());
    assert!(fixture.public().is_file());
    assert!(
        fixture.journal().is_file(),
        "bootstrap owes ownership to installed setup"
    );
    assert!(!fixture.manifest().exists());
}

#[test]
fn a_bootstrap_killed_before_replacing_existing_ck_does_not_report_a_false_success() {
    let fixture = Fixture::new();
    let old = b"pre-journal ck";
    seed_managed_public(&fixture, old);
    fixture
        .kill(fixture.install(), "bootstrap-write-ahead")
        .unwrap();
    assert_eq!(fs::read(fixture.public()).unwrap(), old);
    ok(fixture.install().output().unwrap());
    assert!(
        fs::read(fixture.public()).unwrap() == fs::read(&fixture.extracted).unwrap(),
        "helper did not publish the new executable"
    );
    let repeated = fixture.install().output().unwrap();
    assert!(
        !repeated.status.success(),
        "a published handover requires installed ck setup"
    );
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("run ck setup"));
}

fn seed_managed_public(fixture: &Fixture, before: &[u8]) {
    fs::create_dir_all(fixture.public().parent().unwrap()).unwrap();
    fs::write(fixture.public(), before).unwrap();
    let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(before));
    fs::write(
        fixture.manifest(),
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "platform": if cfg!(windows) {"windows-x64"} else {"linux-x64"},
            "mutations": [{"kind": "managed-binary", "path": fixture.public(), "sha256": digest}]
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn bootstrap_kill_after_publication_finishes_handover_when_setup_starts() {
    let fixture = Fixture::new();
    fixture.kill(fixture.install(), "bootstrap-act").unwrap();
    assert!(fixture.public().is_file());
    assert!(!fixture.manifest().exists());
    // An invalid local release URL makes setup stop after recovery, without
    // talking to a live daemon or starting a service in this test.
    let output = fixture
        .command(&fixture.public())
        .arg("setup")
        .env("CK_TEST_SETUP_CONTROL_OK", "1")
        .env("CK_RELEASE_INDEX_URL", "http://127.0.0.1:0/missing")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fs::read(fixture.public()).unwrap(),
        fs::read(&fixture.extracted).unwrap()
    );
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.manifest()).unwrap()).unwrap();
    let rows = document["mutations"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["kind"] == "managed-binary"
        && row["archive_sha256"] == "a".repeat(64)
        && row["version"] == "1.2.3"));
    assert!(!fixture.journal().exists());
}

#[test]
fn setup_adopts_the_digest_verified_sidecar_left_by_a_released_reinstall() {
    let fixture = Fixture::new();
    let public = fixture.public();
    fs::create_dir_all(public.parent().unwrap()).unwrap();
    common::copy_executable(&fixture.extracted, &public);
    let actual = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(fs::read(&public).unwrap())
    );
    let sidecar = fixture.data().join("installer-manifest.bootstrap.json");
    let document = |rows| {
        serde_json::to_vec(
            &serde_json::json!({"schema_version": 1, "installer": "ck", "mutations": rows}),
        )
        .unwrap()
    };
    fs::write(
        fixture.manifest(),
        document(serde_json::json!([
            {"kind":"managed-binary","path":public,"sha256":"older digest"},
            {"kind":"configuration","path":fixture.data().join("daemon.jsonc")}
        ])),
    )
    .unwrap();
    let backup = fixture.data().join("retired-old-ck");
    let outside = fixture.root.path().join("outside-ck");
    fs::write(&backup, "old ck").unwrap();
    fs::write(&outside, "old ck").unwrap();
    let backup_digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(b"old ck"));
    let retired = serde_json::json!({"kind":"legacy-cli-retirement", "path":backup, "backup_path":backup, "original_path":fixture.data().join("bin").join(binary()), "sha256":backup_digest});
    let profile = serde_json::json!({"kind":"shell-profile-path", "path":fixture.root.path().join(".bashrc"), "shell":"bash"});
    let registry = serde_json::json!({"kind":"user-path-registry", "path":public.parent().unwrap(), "registry_key":r"HKCU\Environment", "registry_value":"Path"});
    fs::write(&sidecar, document(serde_json::json!([
        {"kind":"binary-placement","path":public,"sha256":actual,"archive_sha256":"34".repeat(32)},
        {"kind":"ownership-record","path":fixture.manifest()},
        retired, profile, registry,
        {"kind":"legacy-cli-retirement", "path":outside, "backup_path":outside, "sha256":backup_digest}
    ]))).unwrap();
    let output = fixture
        .command(&public)
        .arg("setup")
        .env("CK_TEST_SETUP_CONTROL_OK", "1")
        .env("CK_RELEASE_INDEX_URL", "http://127.0.0.1:0/missing")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        !sidecar.exists(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.manifest()).unwrap()).unwrap();
    let rows = recorded["mutations"].as_array().unwrap();
    assert!(rows
        .iter()
        .any(|row| row["sha256"] == actual && row["archive_sha256"] == "34".repeat(32)));
    assert!(!rows.iter().any(|row| row["sha256"] == "older digest"));
    assert!(rows.iter().any(|row| row["kind"] == "configuration"));
    for expected in [retired, profile, registry] {
        assert!(
            rows.contains(&expected),
            "missing verified released row {expected}"
        );
    }
    assert!(!rows
        .iter()
        .any(|row| row["path"] == outside.to_string_lossy().as_ref()));
    assert!(outside.is_file());
}

#[cfg(windows)]
#[test]
fn verified_helper_rolls_back_and_retries_a_handover_killed_in_the_rename_gap() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.public().parent().unwrap()).unwrap();
    let old = b"previous ck image";
    fs::write(fixture.public(), old).unwrap();
    let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(old));
    fs::write(
        fixture.manifest(),
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "platform": "windows-x64",
            "mutations": [{"kind": "managed-binary", "path": fixture.public(), "sha256": digest}]
        }))
        .unwrap(),
    )
    .unwrap();

    fixture
        .kill(fixture.install(), "windows-replacement-gap")
        .unwrap();
    assert!(!fixture.public().exists());
    ok(fixture.install().output().unwrap());
    assert_eq!(
        fs::read(fixture.public()).unwrap(),
        fs::read(&fixture.extracted).unwrap()
    );
    assert!(
        fixture.journal().exists(),
        "the retried handover awaits setup"
    );
}
