#![cfg(target_os = "linux")]

use std::{fs, path::Path, process::Command, sync::Arc, time::Duration};

use cortexkit_test_support::{ckdev_binary, ScratchDir};
use serde_json::{json, Value};
use subc_client_rs::{CallOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::{BindIdentity, Flags, Frame, FrameType, Priority, RouteTarget};
use subc_transport::{
    authenticate_client_with_role, connection_file::read_with_local_endpoint, read_frame,
    write_frame, LocalEndpoint,
};
use tokio::{
    net::UnixStream,
    time::{sleep, timeout},
};

const ADAPTER: &str = "mcp-stdio-adapter";
const BUDGET: Duration = Duration::from_secs(15);

#[test]
fn managed_gate_keeps_the_approved_child_until_the_user_approves_the_change() {
    let rig = ScratchDir::new("mcp-gate");
    for folder in [
        "home",
        "data",
        "run",
        "logs",
        "config/cortexkit",
        "authority",
    ] {
        fs::create_dir_all(rig.join(folder)).unwrap();
    }
    let executable = ckdev_binary(std::env::current_exe().unwrap());
    let config = rig.join("config/cortexkit");
    let daemon = json!({"version":1,"port":0,"modules":{ADAPTER:{
        "program": ckdev_binary(env!("CARGO_BIN_EXE_ck-mcp-stdio-adapter")),
        "reserved":true,"registration_channel":true,
        "env":{"XDG_CONFIG_HOME":rig.join("config"),"XDG_DATA_HOME":rig.join("data"),"XDG_RUNTIME_DIR":rig.join("run")}
    }}});
    fs::write(config.join("subc.jsonc"), daemon.to_string()).unwrap();
    fs::write(
        config.join("mcp-servers.jsonc"),
        json!({"fixture": server(env!("CARGO_BIN_EXE_fake-mcp-child"), "old")}).to_string(),
    )
    .unwrap();
    fs::write(rig.join("operator"), "approve\n".repeat(2)).unwrap();
    let output = Command::new(executable)
        .args([
            "--exact",
            "isolated_gate_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("ADAPTER_GATE_FIXTURE", rig.path())
        .env("HOME", rig.join("home"))
        .env("XDG_CONFIG_HOME", rig.join("config"))
        .env("XDG_DATA_HOME", rig.join("data"))
        .env("XDG_RUNTIME_DIR", rig.join("run"))
        .env("SUBC_CONNECTION_FILE", rig.join("run/subc-connection.json"))
        .env("SUBC_TEST_CONFIG_GATE", "1")
        .env("SUBC_TEST_OPERATOR_SCRIPT", rig.join("operator"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn server(command: &str, level: &str) -> Value {
    json!({"command":command,"env":{"LEVEL":{"value":level}}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run in the isolated test daemon process"]
async fn isolated_gate_fixture() {
    let root =
        std::path::PathBuf::from(std::env::var_os("ADAPTER_GATE_FIXTURE").expect("isolated rig"));
    let connection = root.join("run/subc-connection.json");
    let bootstrap = subc_daemon::bootstrap::BootstrapConfig::from_env_for_daemon_binary()
        .unwrap()
        .with_local_listener_socket_dir(root.join("s"))
        .with_capture_logs_dir(root.join("logs"))
        .with_config_store_root(root.join("authority"))
        .unwrap();
    let daemon = tokio::spawn(subc_daemon::bootstrap::run_with_config(bootstrap));
    let client = timeout(BUDGET, async {
        loop {
            if let Ok(client) = SubcConsumer::connect(&connection, ConsumerOptions::default()).await
            {
                break client;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let client = Arc::new(client);
    let result = tokio::spawn({
        let (client, root) = (Arc::clone(&client), root.clone());
        async move { timeout(BUDGET, check_gate(&client, &connection, &root)).await }
    })
    .await;
    if !matches!(result, Ok(Ok(()))) {
        eprintln!("supervisor: {:?}", client.supervisor_list().await);
        eprintln!(
            "adapter: {}",
            fs::read_to_string(root.join("logs/mcp-stdio-adapter.stderr.log")).unwrap_or_default()
        );
    }
    let stop = client.supervisor_set_enabled(ADAPTER, false).await;
    client.close().await;
    daemon.abort();
    let _ = daemon.await;
    result
        .expect("managed gate assertion failed")
        .expect("managed gate scenario timed out");
    stop.unwrap();
}

async fn check_gate(client: &SubcConsumer, connection: &Path, root: &Path) {
    let published = read_with_local_endpoint(connection).unwrap();
    let Some(LocalEndpoint::Unix { path }) = published.local_endpoint else {
        panic!("local socket required")
    };
    let mut desk = UnixStream::connect(path).await.unwrap();
    authenticate_client_with_role(&mut desk, &published.info, BUDGET, "desktop-app")
        .await
        .unwrap();
    wait_for_child(client, "old").await;
    let legacy = root.join("config/cortexkit/mcp-servers.jsonc");
    let user = root.join("config/cortexkit/mcp-stdio-adapter.jsonc");
    assert!(!legacy.exists());
    let stamped = review(&mut desk, 1, "config.stamp").await;
    let stamp = rpc(
        &mut desk,
        2,
        json!({"op":"config.apply","digest":stamped["digest"]}),
    )
    .await;
    assert_eq!(stamp["generation"], 1, "{stamp}");
    wait_for_child(client, "old").await;

    let replacement = root.join("replacement-child");
    fs::copy(env!("CARGO_BIN_EXE_fake-mcp-child"), &replacement).unwrap();
    fs::remove_file(&user).unwrap();
    fs::write(
        &legacy,
        json!({"fixture":server(replacement.to_str().unwrap(), "new")}).to_string(),
    )
    .unwrap();
    client.supervisor_restart(ADAPTER, None).await.unwrap();
    while legacy.exists() {
        sleep(Duration::from_millis(20)).await;
    }
    assert!(user.is_file());
    let original = wait_for_child(client, "old").await;
    let plan = review(&mut desk, 3, "config.plan").await;
    let fields = plan["plan"]["changeset"]["fields"].as_array().unwrap();
    assert!(fields
        .iter()
        .any(|field| field["path"] == "/servers/fixture/command"
            && field["apply"] == "live"
            && field["new"]["value"] == replacement.to_str().unwrap()));
    let before_approval = child(client).await;
    assert_eq!(before_approval["environment"]["LEVEL"], "old");
    assert_eq!(before_approval["pid"], original["pid"]);
    let approved = rpc(
        &mut desk,
        4,
        json!({"op":"config.apply","digest":plan["digest"]}),
    )
    .await;
    assert_eq!(approved["generation"], 2, "{approved}");
    let after_approval = wait_for_child(client, "new").await;
    assert_ne!(after_approval["pid"], original["pid"]);
}

async fn rpc(desk: &mut UnixStream, corr: u64, request: Value) -> Value {
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        request.to_string().into_bytes(),
    )
    .unwrap();
    write_frame(desk, &frame).await.unwrap();
    let reply = read_frame(desk).await.unwrap().unwrap();
    assert_eq!(reply.header.corr, corr);
    serde_json::from_slice(&reply.body).unwrap()
}

async fn review(desk: &mut UnixStream, corr: u64, operation: &str) -> Value {
    let reply = rpc(desk, corr, json!({"op":operation})).await;
    assert!(reply["view"]["digest"].is_string(), "{operation}: {reply}");
    reply["view"].clone()
}

async fn child(client: &SubcConsumer) -> Value {
    let reply = client
        .call(
            RouteTarget::ManagementSurface {
                module_id: ADAPTER.into(),
            },
            BindIdentity::new("adapter-test", "managed-gate"),
            json!({"server":"fixture","op":"tools/call","payload":{"method":"tools/call"}})
                .to_string()
                .into_bytes(),
            CallOptions {
                timeout: Duration::from_secs(1),
                ..CallOptions::default()
            },
        )
        .await;
    reply
        .map(|body| serde_json::from_slice::<Value>(&body).unwrap()["payload"].clone())
        .unwrap_or(Value::Null)
}

async fn wait_for_child(client: &SubcConsumer, level: &str) -> Value {
    loop {
        let current = child(client).await;
        if current["environment"]["LEVEL"] == level {
            return current;
        }
        sleep(Duration::from_millis(20)).await;
    }
}
