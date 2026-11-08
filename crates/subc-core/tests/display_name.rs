#![cfg(unix)]

use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::json;
use std::{
    fs,
    process::{Child, Stdio},
    time::Duration,
};
use subc_client_rs::{HandlerOutcome, ModuleHandler, RequestCtx, SubcConsumer};
use subc_protocol::manifest::ModuleManifest;
use tokio::time::{sleep, timeout};

struct Daemon {
    root: ScratchDir,
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!(
                "daemon log: {}",
                fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
            );
        }
    }
}

struct Observer;

#[async_trait::async_trait]
impl ModuleHandler for Observer {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(Vec::new())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_name_registered_manifest_is_projected_and_absence_stays_omitted() {
    let root = ScratchDir::new("catalog-display-name");
    for dir in ["config/cortexkit", "runtime", "data"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(
        root.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&json!({"version": 1, "modules": {}})).unwrap(),
    )
    .unwrap();
    // Isolate the child itself, not process-global environment in the test runner.
    let child = dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck-subc")))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(root.join("daemon.log")).unwrap())
        .spawn()
        .unwrap();
    let daemon = Daemon { root, child };
    let connection = daemon
        .root
        .join("runtime")
        .join(subc_transport::CONNECTION_FILE_NAME);
    let consumer = timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(consumer) = SubcConsumer::connect(&connection, Default::default()).await {
                break consumer;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("isolated daemon boots");

    let mut serving_tasks = Vec::new();
    for (id, name) in [
        ("named-provider", Some("Routing")),
        ("plain-provider", None),
    ] {
        let manifest = ModuleManifest::builder(id, "1.0.0")
            .display_name(name.map(str::to_string))
            .unwrap()
            .build();
        let (_handle, serving) = subc_client_rs::serve_with_handle(&connection, manifest, Observer)
            .await
            .expect("valid manifest registers");
        serving_tasks.push(tokio::spawn(serving));
    }
    let catalog = consumer.catalog_list().await.unwrap();
    for (id, expected) in [
        ("named-provider", Some("Routing")),
        ("plain-provider", None),
    ] {
        let entry = catalog
            .modules
            .iter()
            .find(|entry| entry.module_id == id)
            .unwrap();
        assert_eq!(entry.display_name.as_deref(), expected);
        let wire = serde_json::to_value(entry).unwrap();
        match expected {
            Some(name) => assert_eq!(wire["display_name"], name),
            None => assert!(wire.get("display_name").is_none()),
        }
    }
    for task in serving_tasks {
        task.abort();
    }
}
