#![cfg(unix)]

use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::{json, Value};
use std::{
    fs,
    process::{Child, Stdio},
    time::Duration,
};
use subc_client_rs::{CallOptions, SubcConsumer};
use subc_protocol::{scope::ScopeSelector, BindIdentity, Principal, RouteTarget};
use tokio::time::{sleep, timeout};

const OWNER: &str = "served-owner";
const PROVIDER: &str = "served-provider";

struct Fixture {
    root: ScratchDir,
    child: Child,
    consumer: SubcConsumer,
    corr: u64,
    generation: u64,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!(
                "{}",
                fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
            );
        }
    }
}

fn spawn(root: &ScratchDir) -> Child {
    dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck-subc")))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("daemon.log"))
                .unwrap(),
        )
        .spawn()
        .unwrap()
}

impl Fixture {
    async fn new(provider_env: Value) -> Self {
        let root = ScratchDir::new("scope-served-by");
        for dir in ["config/cortexkit", "runtime", "data"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let stub = ckdev_binary(env!("CARGO_BIN_EXE_fake-aft-stub"));
        let module = |id: &str| {
            let mut env = json!({
                "FAKE_AFT_MODULE_ID":id,
                "FAKE_AFT_EVENTS_PATH":root.join(format!("{id}.jsonl")),
                "FAKE_AFT_CONTROL_PATH":root.join(format!("{id}.request"))
            });
            if id == PROVIDER {
                env.as_object_mut()
                    .unwrap()
                    .extend(provider_env.as_object().unwrap().clone());
            }
            json!({"program":stub, "args":[], "env":env, "enabled":true, "reserved":id == OWNER})
        };
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&json!({
                "version":1, "modules":{OWNER:module(OWNER), PROVIDER:module(PROVIDER)}
            }))
            .unwrap(),
        )
        .unwrap();
        let child = spawn(&root);
        let consumer = Self::connect(&root).await;
        Self {
            root,
            child,
            consumer,
            corr: 2_000_000,
            generation: 0,
        }
    }

    async fn connect(root: &ScratchDir) -> SubcConsumer {
        timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(client) = SubcConsumer::connect(
                    &root
                        .join("runtime")
                        .join(subc_transport::CONNECTION_FILE_NAME),
                    Default::default(),
                )
                .await
                {
                    if client.catalog_list().await.is_ok_and(|catalog| {
                        [OWNER, PROVIDER]
                            .iter()
                            .all(|id| catalog.modules.iter().any(|m| m.module_id == *id))
                    }) {
                        return client;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("isolated daemon and modules register")
    }

    async fn request(&mut self, caller: &str, body: Value) -> Value {
        self.corr += 1;
        fs::write(
            self.root.join("request.staging"),
            serde_json::to_vec(&json!({"corr":self.corr,"body":body})).unwrap(),
        )
        .unwrap();
        fs::rename(
            self.root.join("request.staging"),
            self.root.join(format!("{caller}.request")),
        )
        .unwrap();
        timeout(Duration::from_secs(10), async {
            loop {
                for line in fs::read_to_string(self.root.join(format!("{caller}.jsonl")))
                    .unwrap_or_default()
                    .lines()
                {
                    if let Ok(event) = serde_json::from_str::<Value>(line) {
                        if event["corr"] == self.corr
                            && (event["kind"] == "control_response" || event["kind"] == "error")
                        {
                            assert_eq!(event["kind"], "control_response", "{event}");
                            return event["body_json"].clone();
                        }
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("registered module control response")
    }

    async fn sync(&mut self, epoch: Option<u64>) {
        self.generation += 1;
        let scopes = epoch.map(|epoch| vec![json!({"ref":"session", "scope_epoch":epoch, "kind":"head", "carriers":[{"principal":{"kind":"direct"}}]})]).unwrap_or_default();
        let reply = self
            .request(
                OWNER,
                json!({"op":"scope.sync", "generation":self.generation, "scopes":scopes}),
            )
            .await;
        assert_eq!(reply["op"], "scope.sync");
        if epoch.is_some() {
            assert_ne!(reply["results"][0]["outcome"], "refused", "{reply}");
        }
    }

    async fn served(&mut self, caller: &str, epoch: u64, module: &str) -> Value {
        let reply = self.request(caller, json!({"op":"scope.served_by", "scope_ref":"session", "scope_epoch":epoch, "module_id":module})).await;
        assert_eq!(reply["op"], "scope.served_by");
        reply
    }

    async fn open(&self, epoch: u64) -> Result<(), subc_client_rs::CallError> {
        let handle = self
            .consumer
            .open_route_scoped(
                RouteTarget::ToolProvider {
                    module_id: PROVIDER.into(),
                },
                BindIdentity::new("served-test", "session"),
                ScopeSelector::new(
                    Principal::Reserved {
                        module_id: OWNER.into(),
                    },
                    "session",
                    Some(epoch),
                ),
                CallOptions {
                    timeout: Duration::from_secs(2),
                    route_retry_deadline: Duration::from_secs(1),
                    ..Default::default()
                },
            )
            .await?;
        self.consumer
            .close_handle(&handle, Default::default())
            .await?;
        timeout(Duration::from_secs(5), async {
            while !self.saw_bind("detach") {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("target observes route closure before history lookup");
        Ok(())
    }

    fn saw_bind(&self, kind: &str) -> bool {
        fs::read_to_string(self.root.join(format!("{PROVIDER}.jsonl")))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["kind"] == kind)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_by_acknowledged_bind_and_owner_namespace() {
    let mut f = Fixture::new(json!({})).await;
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "unknown");
    f.sync(Some(1)).await;
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "not_served");
    f.open(1).await.unwrap();
    assert!(f.saw_bind("attach"));
    let served = f.served(OWNER, 1, PROVIDER).await;
    assert_eq!(served["answer"], "served");
    assert_eq!(f.served(OWNER, 1, OWNER).await["answer"], "not_served");
    assert_eq!(f.served(PROVIDER, 1, PROVIDER).await["answer"], "unknown");
    let described = f.request(OWNER, json!({"op":"scope.describe", "owner":{"kind":"reserved","module_id":OWNER}, "ref":"session"})).await;
    assert_eq!(described["op"], "scope.describe");
    assert_eq!(
        served["daemon_incarnation"],
        described["daemon_incarnation"]
    );
    // Same-epoch sync preserves history even after the consumer closed its route.
    f.sync(Some(1)).await;
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "served");
    f.sync(Some(2)).await;
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "unknown");
    assert_eq!(f.served(OWNER, 2, PROVIDER).await["answer"], "not_served");
    f.sync(None).await;
    assert_eq!(f.served(OWNER, 2, PROVIDER).await["answer"], "unknown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_by_refused_bind_stays_not_served() {
    let mut f = Fixture::new(json!({"FAKE_AFT_REJECT_ATTACH":"1"})).await;
    f.sync(Some(1)).await;
    assert!(f.open(1).await.is_err());
    assert!(
        f.saw_bind("attach"),
        "target must actually receive and refuse the bind"
    );
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "not_served");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_by_unacknowledged_bind_stays_not_served() {
    let mut f = Fixture::new(json!({"FAKE_AFT_BIND_NEVER_REPLY":"1"})).await;
    f.sync(Some(1)).await;
    assert!(f.open(1).await.is_err());
    assert!(f.saw_bind("attach_never_reply"));
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "not_served");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_by_restart_forgets_until_owner_registers_epoch_again() {
    let mut f = Fixture::new(json!({})).await;
    f.sync(Some(1)).await;
    f.open(1).await.unwrap();
    let before = f.served(OWNER, 1, PROVIDER).await;
    assert_eq!(before["answer"], "served");
    f.child.kill().unwrap();
    f.child.wait().unwrap();
    for id in [OWNER, PROVIDER] {
        let _ = fs::remove_file(f.root.join(format!("{id}.request")));
    }
    f.child = spawn(&f.root);
    f.consumer = Fixture::connect(&f.root).await;
    let after = f.served(OWNER, 1, PROVIDER).await;
    assert_ne!(before["daemon_incarnation"], after["daemon_incarnation"]);
    assert_eq!(after["answer"], "unknown");
    f.sync(Some(1)).await;
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "not_served");
}
