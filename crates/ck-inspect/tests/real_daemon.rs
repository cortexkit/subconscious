#![cfg(any(target_os = "macos", target_os = "linux"))]

use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use subc_client_rs::{CallOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

struct Daemon {
    root: tempfile::TempDir,
    child: Child,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for name in ["inspect", "load"] {
            if let Ok(text) = fs::read_to_string(self.root.path().join(format!("{name}.pid"))) {
                if let Ok(id) = text.trim().parse() {
                    if let Some(pid) = rustix::process::Pid::from_raw(id) {
                        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stripped_helper_profile_uses_module_dsym_symbols() {
    for tool in ["/usr/bin/dsymutil", "/usr/bin/atos"] {
        assert!(
            std::path::Path::new(tool).is_file(),
            "live symbolization test requires {tool}"
        );
    }
    let root = tempfile::Builder::new()
        .prefix("cki-dsym-")
        .tempdir()
        .unwrap();
    for dir in ["config/cortexkit", "runtime", "data"] {
        fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    fs::set_permissions(
        root.path().join("runtime"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let source = root.path().join("helper.c");
    fs::write(&source, include_str!("fixtures/stripped-helper.c")).unwrap();
    let build = root.path().join("helper-build");
    let run = |command: &mut Command| {
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("cannot run {command:?}: {error}"));
        assert!(
            output.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    run(Command::new("/usr/bin/clang")
        .args(["-g", "-O0"])
        .arg(&source)
        .arg("-o")
        .arg(&build));
    let uuid_output = run(Command::new("/usr/bin/dwarfdump").arg("--uuid").arg(&build));
    let uuid = String::from_utf8(uuid_output.stdout)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .expect("dwarfdump must print a UUID")
        .replace('-', "")
        .to_ascii_uppercase();
    assert_eq!(uuid.len(), 32);
    let bundle = root
        .path()
        .join("data/cortexkit/load/dsym")
        .join(uuid)
        .join("symbols.dSYM");
    fs::create_dir_all(bundle.parent().unwrap()).unwrap();
    run(Command::new("/usr/bin/dsymutil")
        .arg(&build)
        .arg("-o")
        .arg(&bundle));
    let helper = root.path().join("busy-helper");
    fs::copy(build, &helper).unwrap();
    run(Command::new("/usr/bin/strip").arg(&helper));
    let symbols = run(Command::new("/usr/bin/nm").arg(&helper));
    assert!(
        !String::from_utf8_lossy(&symbols.stdout).contains("inspect_busy_loop"),
        "busy function must not remain in the stripped symbol table"
    );
    let config = json!({"version":1, "modules":{
        "inspect": {"program":env!("CARGO_BIN_EXE_ck-inspect"), "args":[], "env":{}},
        "load": {"protocol":"none", "program":helper, "args":[],
            "env":{"INSPECT_FIXTURE_PID_PATH":root.path().join("load.pid")}}
    }});
    fs::write(
        root.path().join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let daemon_bin = std::env::var_os("CK_INSPECT_TEST_DAEMON")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_BIN_EXE_ck-inspect")).with_file_name("ck-subc")
        });
    assert!(
        daemon_bin.exists(),
        "build subc-core --bin ck-subc before the live test: {}",
        daemon_bin.display()
    );
    let log = fs::File::create(root.path().join("daemon.log")).unwrap();
    let child = Command::new(daemon_bin)
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .env_remove("SUBC_CONNECTION_FILE")
        .env_remove("CK_LOG")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut daemon = Daemon { root, child };
    let connection = daemon.root.path().join("runtime/subc-connection.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !connection.exists() {
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            fs::read_to_string(daemon.root.path().join("daemon.log")).unwrap()
        );
        assert!(Instant::now() < deadline, "connection file never appeared");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let consumer = SubcConsumer::connect(&connection, ConsumerOptions::default())
        .await
        .unwrap();
    loop {
        let list = consumer.supervisor_list().await.unwrap();
        if ["inspect", "load"].iter().all(|id| {
            list.modules
                .iter()
                .any(|entry| entry.module_id == *id && entry.live)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "modules never became live: {list:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let snapshot = consumer.spawn_snapshot().await.unwrap();
    let inspect = snapshot
        .live
        .iter()
        .find(|s| s.module_id == "inspect")
        .unwrap();
    fs::write(
        daemon.root.path().join("inspect.pid"),
        inspect.pid.to_string(),
    )
    .unwrap();
    let reply = consumer
        .call(
            RouteTarget::ManagementSurface {
                module_id: "inspect".into(),
            },
            BindIdentity::new("inspect-test", "symbols"),
            serde_json::to_vec(
                &json!({"method":"module.profile", "params":{"module_id":"load", "seconds":1}}),
            )
            .unwrap(),
            CallOptions::default(),
        )
        .await
        .unwrap();
    assert!(reply.len() <= 16 * 1024);
    let profile: Value = serde_json::from_slice(&reply).unwrap();
    let frames: Vec<_> = profile["result"]["threads"]
        .as_array()
        .expect("profile must succeed")
        .iter()
        .flat_map(|thread| thread["stacks"].as_array().unwrap())
        .flat_map(|stack| stack["frames"].as_array().unwrap())
        .filter_map(Value::as_str)
        .collect();
    assert!(
        frames.contains(&"inspect_busy_loop"),
        "stripped busy function must be named from its separate dSYM: {profile}"
    );
    println!("live dSYM named frame: inspect_busy_loop");
    consumer.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routed_resources_and_profile_observe_supervised_process_and_exit_on_eof() {
    let root = tempfile::Builder::new().prefix("cki-").tempdir().unwrap();
    for dir in ["config/cortexkit", "runtime", "data"] {
        fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    fs::set_permissions(
        root.path().join("runtime"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let config = json!({"version":1, "modules":{
        "inspect": {"program":env!("CARGO_BIN_EXE_ck-inspect"), "args":[], "env":{}},
        "load": {"protocol":"none", "program":env!("CARGO_BIN_EXE_inspect-load-fixture"), "args":[],
            "env":{"INSPECT_FIXTURE_PID_PATH":root.path().join("load.pid")}}
    }});
    fs::write(
        root.path().join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let daemon_bin = std::env::var_os("CK_INSPECT_TEST_DAEMON")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_BIN_EXE_ck-inspect")).with_file_name("ck-subc")
        });
    assert!(
        daemon_bin.exists(),
        "build subc-core --bins before running the real daemon test: {}",
        daemon_bin.display()
    );
    let log = fs::File::create(root.path().join("daemon.log")).unwrap();
    let child = Command::new(daemon_bin)
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .env_remove("SUBC_CONNECTION_FILE")
        .env_remove("CK_LOG")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut daemon = Daemon { root, child };
    let connection = daemon.root.path().join("runtime/subc-connection.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !connection.exists() {
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            fs::read_to_string(daemon.root.path().join("daemon.log")).unwrap()
        );
        assert!(Instant::now() < deadline, "connection file never appeared");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let consumer = SubcConsumer::connect(&connection, ConsumerOptions::default())
        .await
        .unwrap();
    loop {
        let list = consumer.supervisor_list().await.unwrap();
        if list
            .modules
            .iter()
            .any(|e| e.module_id == "inspect" && e.live)
            && list.modules.iter().any(|e| e.module_id == "load" && e.live)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "modules never became live: {list:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let target = RouteTarget::ManagementSurface {
        module_id: "inspect".into(),
    };
    let snapshot = consumer.spawn_snapshot().await.unwrap();
    let inspect = snapshot
        .live
        .iter()
        .find(|s| s.module_id == "inspect")
        .unwrap();
    fs::write(
        daemon.root.path().join("inspect.pid"),
        inspect.pid.to_string(),
    )
    .unwrap();
    let identity = BindIdentity::new("inspect-test", "profile");
    let resources = consumer
        .call(
            target.clone(),
            identity.clone(),
            serde_json::to_vec(&json!({"method":"module.resources","params":{}})).unwrap(),
            CallOptions::default(),
        )
        .await
        .unwrap();
    let mut resources: Value = serde_json::from_slice(&resources).unwrap();
    let resources = resources["result"].take();
    assert!(resources.is_object());
    let load = resources["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["module_id"] == "load")
        .unwrap();
    assert!(load["pid"].as_u64().unwrap() > 0);
    assert!(load["resources"].is_object());
    let result = consumer
        .call(
            target,
            identity,
            serde_json::to_vec(
                &json!({"method":"module.profile","params":{"module_id":"load","seconds":1}}),
            )
            .unwrap(),
            CallOptions::default(),
        )
        .await
        .unwrap();
    assert!(result.len() <= 16 * 1024);
    let mut profile: Value = serde_json::from_slice(&result).unwrap();
    let profile = profile["result"].take();
    assert_eq!(profile["pid"], load["pid"]);
    let threads = profile["threads"].as_array().unwrap();
    #[cfg(target_os = "macos")]
    {
        assert_eq!(profile["method"], "sample");
        assert!(
            threads
                .iter()
                .any(|t| t["name"].as_str().unwrap().contains("profile-worker")
                    && t["samples"].as_u64().unwrap() > 0),
            "{profile}"
        );
        assert!(profile["idle_threads"].as_u64().unwrap() >= 1, "{profile}");
    }
    #[cfg(target_os = "linux")]
    {
        assert_eq!(profile["method"], "proc_threads");
        assert_eq!(threads[0]["name"], "profile-worker");
        assert!(threads[0]["cpu_ms"].as_u64().unwrap() > 0);
    }
    if let Some(path) = std::env::var_os("CK_INSPECT_TEST_EXAMPLE") {
        fs::write(
            path,
            serde_json::to_vec_pretty(&json!({"resources":resources,"profile":profile})).unwrap(),
        )
        .unwrap();
    }
    let inspect_pid: i32 = fs::read_to_string(daemon.root.path().join("inspect.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    let pid = rustix::process::Pid::from_raw(inspect_pid).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while rustix::process::test_kill_process(pid).is_ok() {
        #[cfg(target_os = "linux")]
        if fs::read_to_string(format!("/proc/{inspect_pid}/stat"))
            .is_ok_and(|s| s.rsplit_once(')').unwrap().1.trim_start().starts_with('Z'))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "inspect did not exit after daemon EOF"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    consumer.close().await;
}
