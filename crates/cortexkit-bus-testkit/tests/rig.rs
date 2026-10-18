#![cfg(unix)]

use cortexkit_bus_testkit::{
    skip_or_fail, BusOptions, BusServer, DaemonOptions, Module, PrivateDaemon, RigError,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime},
};
use subc_control::{ClientControlRequest, ClientControlResponse};

const LIMIT: Duration = Duration::from_secs(30);

fn prerequisites() -> bool {
    for result in [
        cortexkit_bus_testkit::ck_subc_binary(),
        cortexkit_bus_testkit::ck_bus_binary(),
        cortexkit_bus_testkit::nats_server_bin().map(|(path, _)| path),
    ] {
        if let Err(error) = result {
            skip_or_fail(error);
            return false;
        }
    }
    true
}

async fn bus() -> BusServer {
    BusServer::start(BusOptions::default()).await.unwrap()
}

async fn wait_file(path: &Path, daemon: Option<&PrivateDaemon>) {
    let deadline = Instant::now() + LIMIT;
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "file never appeared: {}; {}",
            path.display(),
            daemon.map(PrivateDaemon::log_text).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn binary_policy_probe() {
    let Ok(which) = std::env::var("TESTKIT_BINARY_PROBE") else {
        return;
    };
    let error = match which.as_str() {
        "nats" => cortexkit_bus_testkit::nats_server_bin().err().unwrap(),
        "bus" => cortexkit_bus_testkit::ck_bus_binary().unwrap_err(),
        "subc" => cortexkit_bus_testkit::ck_subc_binary().unwrap_err(),
        _ => panic!("unknown probe"),
    };
    let variable = match which.as_str() {
        "nats" => "CK_NATS_SERVER_BIN",
        "bus" => "CK_BUS_BIN",
        _ => "CK_SUBC_BIN",
    };
    assert!(
        matches!(&error, RigError::MissingBinary { env_var, how_to_fix, .. }
        if *env_var == variable && how_to_fix.contains(variable))
    );
    assert!(error.to_string().contains(variable));
    let required = std::env::var("CK_BUS_TESTKIT_REQUIRE").as_deref() == Ok("1");
    assert_eq!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| skip_or_fail(error))).is_err(),
        required
    );
    assert!(
        std::panic::catch_unwind(|| skip_or_fail(RigError::Invalid("broken rig".into()))).is_err()
    );
}

#[test]
fn missing_binaries_and_require_policy() {
    let executable = cortexkit_test_support::ckdev_binary(std::env::current_exe().unwrap());
    for which in ["nats", "bus", "subc"] {
        for required in [false, true] {
            let mut command = Command::new(&executable);
            command
                .args(["--exact", "binary_policy_probe", "--nocapture"])
                .env("TESTKIT_BINARY_PROBE", which)
                .env("CK_NATS_SERVER_BIN", "/nonexistent/testkit/nats-server")
                .env_remove("CK_BUS_BIN")
                .env_remove("CK_SUBC_BIN")
                .env_remove("CK_BUS_TESTKIT_REQUIRE");
            if required {
                command.env("CK_BUS_TESTKIT_REQUIRE", "1");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{which} require={required}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
    }
}

fn read_optional(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => panic!("read {}: {error}", path.display()),
    }
}

struct ProtectedFile {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    modified: Option<SystemTime>,
}

impl ProtectedFile {
    fn capture(path: PathBuf, check_mtime: bool) -> Self {
        let (bytes, modified) = match std::fs::File::open(&path) {
            Ok(mut file) => {
                let modified = check_mtime.then(|| file.metadata().unwrap().modified().unwrap());
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).unwrap();
                (Some(bytes), modified)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (None, None),
            Err(error) => panic!("open {}: {error}", path.display()),
        };
        Self {
            path,
            bytes,
            modified,
        }
    }

    fn assert_unchanged(&self) {
        let after = Self::capture(self.path.clone(), self.modified.is_some());
        // Connection files contain authentication keys; never include their bytes
        // in assertion diagnostics.
        assert!(
            self.bytes == after.bytes,
            "production file bytes changed: {}",
            self.path.display()
        );
        assert!(
            self.modified == after.modified,
            "production file mtime changed: {}",
            self.path.display()
        );
    }
}

fn inherited_home(variable: &str, fallback: PathBuf) -> PathBuf {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback)
}

fn production_connection_files() -> Vec<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        return vec![PathBuf::from(runtime).join(subc_transport::CONNECTION_FILE_NAME)];
    }
    vec![
        std::env::temp_dir().join(subc_transport::CONNECTION_FILE_NAME),
        // Current daemons use a per-user fallback when XDG_RUNTIME_DIR is absent.
        std::env::temp_dir().join(format!(
            "subc-{}.connection.json",
            subc_transport::connection_file::user_connection_token()
        )),
    ]
}

fn assert_no_rig_pids(path: &Path, rig_pids: &[u32]) {
    let Some(bytes) = read_optional(path) else {
        return;
    };
    let record: Value = serde_json::from_slice(&bytes).expect("live-children record is JSON");
    for child in record["children"]
        .as_array()
        .expect("live-children has a children array")
    {
        let pid = u32::try_from(child["pid"].as_u64().expect("live child has a pid")).unwrap();
        assert!(
            !rig_pids.contains(&pid),
            "production live-children record {} contains rig pid {pid}",
            path.display()
        );
    }
}

fn assert_no_rig_paths(root: &Path, markers: &[&str]) {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => panic!("scan {}: {error}", directory.display()),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => panic!("scan {}: {error}", directory.display()),
            };
            let path = entry.path();
            for marker in markers {
                assert!(
                    !path.to_string_lossy().contains(marker),
                    "rig marker {marker:?} in production path {}",
                    path.display()
                );
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => directories.push(path),
                // Live modules may remove unrelated files while the scan runs.
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => panic!("inspect {}: {error}", path.display()),
            }
        }
    }
}

async fn wait_private_child_record(daemon: &PrivateDaemon, module_id: &str, pid: u32) {
    let path = daemon.data_home.join("cortexkit/run/live-children.json");
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(bytes) = read_optional(&path) {
            let record: Value = serde_json::from_slice(&bytes).unwrap();
            if record["children"]
                .as_array()
                .unwrap()
                .iter()
                .any(|child| child["module_id"] == module_id && child["pid"] == pid)
            {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "private record never listed {module_id} pid {pid}: {}",
            daemon.log_text()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn production_record_rejects_each_rig_pid() {
    let root = tempfile::tempdir().unwrap();
    let record = root.path().join("live-children.json");
    let rig_pids = [101, 102, 103, 104];
    let write_record = |pid| {
        std::fs::write(
            &record,
            serde_json::to_vec(&json!({
                "version": 1, "children": [{"module_id": "unrelated", "pid": pid}],
            }))
            .unwrap(),
        )
        .unwrap();
    };
    assert_no_rig_pids(&record, &rig_pids);
    write_record(999);
    assert_no_rig_pids(&record, &rig_pids);
    for pid in rig_pids {
        write_record(pid);
        assert!(
            std::panic::catch_unwind(|| assert_no_rig_pids(&record, &rig_pids)).is_err(),
            "production record must reject rig pid {pid}"
        );
    }
}

#[test]
fn production_path_scan_ignores_unrelated_writes_and_rejects_rig_markers() {
    let root = tempfile::tempdir().unwrap();
    let module_id = "bustest-unique-fixture";
    let root_name = ".tmp-unique-fixture";
    let markers = [module_id, root_name];
    assert_no_rig_paths(&root.path().join("absent"), &markers);
    std::fs::create_dir(root.path().join("aft")).unwrap();
    std::fs::write(root.path().join("aft/live.log"), b"first write").unwrap();
    assert_no_rig_paths(root.path(), &markers);
    std::fs::write(root.path().join("aft/another.log"), b"unrelated new file").unwrap();
    std::fs::write(root.path().join("aft/live.log"), b"later write").unwrap();
    assert_no_rig_paths(root.path(), &markers);
    for marker in markers {
        let leaked = root
            .path()
            .join("aft")
            .join(format!("prefix-{marker}-suffix"));
        std::fs::write(&leaked, b"leak").unwrap();
        assert!(
            std::panic::catch_unwind(|| assert_no_rig_paths(root.path(), &markers)).is_err(),
            "production scan must reject marker {marker}"
        );
        std::fs::remove_file(leaked).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_daemon_isolates_and_cleans_up() {
    if !prerequisites() {
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let real_data = inherited_home("XDG_DATA_HOME", home.join(".local/share")).join("cortexkit");
    let real_config =
        inherited_home("XDG_CONFIG_HOME", home.join(".config")).join("cortexkit/subc.jsonc");
    let real_children = real_data.join("run/live-children.json");
    let mut protected = vec![
        ProtectedFile::capture(real_data.join("machine-id"), true),
        ProtectedFile::capture(real_config, false),
    ];
    protected.extend(
        production_connection_files()
            .into_iter()
            .map(|path| ProtectedFile::capture(path, false)),
    );
    let module_id = format!("bustest-{}", cortexkit_bus_testkit::new_machine_id());
    let bus = bus().await;
    let bus_root = bus.dir.clone();
    let bus_pid = bus.pid();
    let daemon = PrivateDaemon::start(
        &bus,
        DaemonOptions {
            modules: vec![test_module(&module_id)],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    wait_file(&daemon.runtime_dir.join("module-homes.json"), Some(&daemon)).await;
    let homes: Value = serde_json::from_slice(
        &std::fs::read(daemon.runtime_dir.join("module-homes.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(homes["data"], daemon.data_home.display().to_string());
    assert_eq!(homes["runtime"], daemon.runtime_dir.display().to_string());
    assert_eq!(homes["config"], daemon.config_home.display().to_string());
    // The daemon allocated a real per-module SQLite descriptor under the private
    // data home, as production does, so a module opening a leased store can run.
    wait_file(
        &daemon.runtime_dir.join("module-storage.json"),
        Some(&daemon),
    )
    .await;
    let storage: Value = serde_json::from_slice(
        &std::fs::read(daemon.runtime_dir.join("module-storage.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        storage["backend"]["backend"], "sqlite",
        "storage descriptor: {storage}"
    );
    assert_eq!(
        storage["backend"]["path"],
        format!(
            "{}/cortexkit/{module_id}/store.db",
            daemon.data_home.display()
        ),
        "storage descriptor: {storage}"
    );
    let root = daemon.root().to_path_buf();
    let pid = daemon.pid();
    let module_pids = [
        supervised_pid(&daemon, "ckbus").await,
        homes["pid"].as_u64().unwrap() as u32,
    ];
    wait_private_child_record(&daemon, &module_id, module_pids[1]).await;
    let rig_pids = [pid, module_pids[0], module_pids[1], bus_pid];
    let root_name = root.file_name().unwrap().to_str().unwrap();
    assert!(
        subc_os::Process::open(module_pids[1]).unwrap().is_some(),
        "probe exited: {}",
        daemon.log_text()
    );
    // Positive evidence matters: clean production paths alone would also pass if
    // the daemon had never started or the identity had been written in the wrong tree.
    assert!(daemon.data_home.join("cortexkit/machine-id").is_file());
    assert!(daemon
        .data_home
        .join("cortexkit/run/live-children.json")
        .is_file());
    assert!(daemon.connection_file.is_file());
    assert!(daemon.connection_file.starts_with(&daemon.runtime_dir));
    assert!(daemon.config_file.is_file());
    assert!(daemon.config_file.starts_with(&daemon.config_home));
    assert!(daemon.runtime_dir.is_dir());
    assert!(!daemon.data_home.starts_with(&real_data));
    // The live fleet owns these production paths too. Only immutable file contents
    // and this rig's unique markers/PIDs identify a leak; unrelated writes do not.
    for file in &protected {
        file.assert_unchanged();
    }
    assert_no_rig_pids(&real_children, &rig_pids);
    assert_no_rig_paths(&real_data, &[&module_id, root_name]);
    drop(daemon);
    assert!(!root.exists(), "daemon tree survives Drop");
    assert!(
        subc_os::Process::open(pid).unwrap().is_none(),
        "daemon survives Drop"
    );
    for pid in module_pids {
        assert!(
            subc_os::Process::open(pid).unwrap().is_none(),
            "supervised module survives Drop"
        );
    }
    drop(bus);
    assert!(!bus_root.exists(), "broker tree survives Drop");
    assert!(
        subc_os::Process::open(bus_pid).unwrap().is_none(),
        "NATS survives Drop"
    );
    for file in &protected {
        file.assert_unchanged();
    }
    assert_no_rig_pids(&real_children, &rig_pids);
    assert_no_rig_paths(&real_data, &[&module_id, root_name]);
}

#[cfg(target_os = "linux")]
fn executable_path(pid: u32) -> PathBuf {
    std::fs::read_link(format!("/proc/{pid}/exe")).unwrap()
}

#[cfg(target_os = "macos")]
fn executable_path(pid: u32) -> PathBuf {
    let output = Command::new(cortexkit_test_support::ckdev_binary("/bin/ps"))
        .args(["-ww", "-p", &pid.to_string(), "-o", "comm="])
        .output()
        .unwrap();
    assert!(output.status.success());
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_executables_are_ckdev_copies() {
    if !prerequisites() {
        return;
    }
    let bus = bus().await;
    let daemon = PrivateDaemon::start(&bus, DaemonOptions::default())
        .await
        .unwrap();
    let bus_pid = supervised_pid(&daemon, "ckbus").await;
    for (pid, staged) in [
        (
            bus.pid(),
            cortexkit_bus_testkit::nats_server_bin().unwrap().0,
        ),
        (
            daemon.pid(),
            cortexkit_bus_testkit::ck_subc_binary().unwrap(),
        ),
        (bus_pid, cortexkit_bus_testkit::ck_bus_binary().unwrap()),
    ] {
        let actual = executable_path(pid);
        assert_eq!(
            actual.canonicalize().unwrap(),
            staged.canonicalize().unwrap()
        );
        assert!(actual
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ckdev-"));
        // Cross-check the kernel's executable inode, not argv or the configured path.
        let observed = subc_os::Process::open(pid)
            .unwrap()
            .unwrap()
            .observe()
            .unwrap();
        assert_eq!(observed.executable, subc_os::file_identity(&actual));
        assert!(observed.executable.is_some());
    }
}

fn test_module(id: &str) -> Module {
    // Cargo test builds examples beside the integration-test deps directory, even
    // with a custom target directory or a release profile. A direct executable
    // preserves the image identity checked by macOS's privacy trampoline.
    let executable = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/bus-test-module");
    assert!(
        executable.is_file(),
        "Cargo must build the bus-test-module example"
    );
    Module::new(id, executable)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_publishes_with_delivered_credential_and_nonce_sign() {
    if !prerequisites() {
        return;
    }
    let result = tempfile::tempdir().unwrap();
    let mut module = test_module("testkit_probe");
    module
        .env
        .insert("TESTKIT_RESULT".into(), result.path().display().to_string());
    let bus = bus().await;
    let daemon = PrivateDaemon::start(
        &bus,
        DaemonOptions {
            modules: vec![module],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    wait_file(&result.path().join("credential.json"), Some(&daemon)).await;
    let credential: Value =
        serde_json::from_slice(&std::fs::read(result.path().join("credential.json")).unwrap())
            .unwrap();
    assert_eq!(credential["server_url"], bus.url);
    let observer_key = nkeys::KeyPair::new_user();
    let jwt = bus.trust.user_jwt(
        &cortexkit_bus_testkit::trust::box_root_id(),
        credential["account_public"].as_str().unwrap(),
        &observer_key,
        cortexkit_bus_testkit::trust::unix_now() - 60,
    );
    let observer = async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
        let signature = observer_key.sign(&nonce);
        async move { signature.map_err(async_nats::AuthError::new) }
    })
    .connect(&bus.url)
    .await
    .unwrap();
    let subject = format!(
        "ck.{}.event.testkit_probe.smoke.v1",
        credential["acct"].as_str().unwrap()
    );
    let mut subscription = observer.subscribe(subject.clone()).await.unwrap();
    observer.flush().await.unwrap();
    std::fs::write(result.path().join("publish-now"), b"ready").unwrap();
    let message = tokio::time::timeout(LIMIT, subscription.next())
        .await
        .unwrap_or_else(|_| panic!("publisher did not deliver: {}", daemon.log_text()))
        .unwrap();
    assert_eq!(message.subject.as_str(), subject);
    assert_eq!(message.payload.as_ref(), b"private smoke");
    drop(daemon);
}

#[test]
fn config_declares_reserved_ckbus_and_caller_modules() {
    let executable = std::env::current_exe().unwrap();
    let mut module = Module::new("publisher", &executable);
    module.args = vec!["argument with spaces".into()];
    module
        .env
        .insert("FIXTURE_VALUE".into(), "quoted \"value\"".into());
    module.reserved = true;
    let text = cortexkit_bus_testkit::subc_config(
        &executable,
        &[("CKBUS_NATS_URL".into(), "nats://127.0.0.1:1234".into())],
        &[module],
    )
    .unwrap();
    let config: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(config["port"], 0);
    assert_eq!(config["modules"]["ckbus"]["reserved"], true);
    assert_eq!(config["modules"]["ckbus"]["protocol"], "subc");
    assert_eq!(
        config["modules"]["ckbus"]["env"]["CKBUS_NATS_URL"],
        "nats://127.0.0.1:1234"
    );
    assert_eq!(
        config["modules"]["publisher"]["args"],
        json!(["argument with spaces"])
    );
    assert_eq!(
        config["modules"]["publisher"]["env"]["FIXTURE_VALUE"],
        "quoted \"value\""
    );
    assert_eq!(config["modules"]["publisher"]["reserved"], true);
    assert_eq!(
        config["modules"]["publisher"]["program"],
        cortexkit_test_support::ckdev_binary(&executable)
            .display()
            .to_string()
    );
    assert!(cortexkit_bus_testkit::subc_config(
        &executable,
        &[],
        &[Module::new("ckbus", &executable)]
    )
    .is_err());
}

#[test]
fn machine_id_is_validated_and_written_under_the_given_home() {
    let root = tempfile::tempdir().unwrap();
    let machine_id = cortexkit_bus_testkit::new_machine_id();
    let path = cortexkit_bus_testkit::write_machine_id(root.path(), &machine_id).unwrap();
    assert_eq!(path, root.path().join("cortexkit/machine-id"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{machine_id}\n")
    );
    assert!(cortexkit_bus_testkit::write_machine_id(root.path(), "not an id").is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{machine_id}\n")
    );
}

async fn supervised_pid(daemon: &PrivateDaemon, module_id: &str) -> u32 {
    let deadline = Instant::now() + LIMIT;
    loop {
        let response = daemon
            .control(ClientControlRequest::SupervisorProvenance {
                module_id: Some(module_id.into()),
            })
            .await
            .unwrap();
        let ClientControlResponse::SupervisorProvenance { modules, .. } = response else {
            panic!("provenance response");
        };
        if let Some(pid) = modules
            .iter()
            .find(|module| module.module_id == module_id)
            .and_then(|module| module.daemon_observed.pid)
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "no supervised pid for {module_id}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
