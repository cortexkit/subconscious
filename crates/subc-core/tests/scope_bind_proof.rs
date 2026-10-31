#![cfg(unix)]

use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    process::{Child, Stdio},
    time::Duration,
};
use subc_client_rs::{
    CallError, CallOptions, ConsumerIdentity, ScopeBindProofRefusal, SubcConsumer,
};
use subc_protocol::{scope::ScopeSelector, BindIdentity, Principal, RouteTarget};
use tokio::time::{sleep, timeout};

const OWNER: &str = "bind-proof-owner";
const PROVIDER: &str = "bind-proof-provider";
const PROOF: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc";
const HASH: &str = "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0";
const NEXT_PROOF: &str = "CAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAg";
const NEXT_HASH: &str = "2578ccf8645b2d1dc10c465eff843585970f3a7e22296a92cad55d489a272072";

struct Fixture {
    root: ScratchDir,
    child: Child,
    consumer: SubcConsumer,
    generation: u64,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "daemon log: {}",
                fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn events(path: PathBuf) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

impl Fixture {
    async fn new() -> Self {
        let root = ScratchDir::new("scope-bind-proof");
        for dir in ["config/cortexkit", "runtime", "data"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let stub = ckdev_binary(env!("CARGO_BIN_EXE_fake-aft-stub"));
        let module = |id: &str, reserved: bool| {
            let mut env = BTreeMap::from([
                ("FAKE_AFT_MODULE_ID", id.to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH",
                    root.join(format!("{id}.jsonl"))
                        .to_string_lossy()
                        .into_owned(),
                ),
            ]);
            if id == OWNER {
                env.insert(
                    "FAKE_AFT_SCOPE_SYNC_PATH",
                    root.join("sync.json").to_string_lossy().into_owned(),
                );
                env.insert(
                    "FAKE_AFT_LAUNCH_NONCE_PATH",
                    root.join("owner.nonce").to_string_lossy().into_owned(),
                );
            }
            json!({"program":stub, "args":[], "env":env, "enabled":true, "reserved":reserved})
        };
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&json!({
                "version":1, "modules":{
                    OWNER: module(OWNER, true), PROVIDER: module(PROVIDER, false),
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let child = dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck-subc")))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("SUBC_PORT", "0")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env("CK_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.join("daemon.log")).unwrap())
            .spawn()
            .unwrap();
        let connection = root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME);
        let consumer = timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(client) = SubcConsumer::connect(&connection, Default::default()).await {
                    break client;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("isolated daemon boots");
        let fixture = Self {
            root,
            child,
            consumer,
            generation: 0,
        };
        timeout(Duration::from_secs(20), async {
            loop {
                let catalog = fixture.consumer.catalog_list().await.unwrap();
                if [OWNER, PROVIDER]
                    .iter()
                    .all(|id| catalog.modules.iter().any(|entry| entry.module_id == *id))
                    && fixture.root.join("owner.nonce").exists()
                {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("owner and provider register");
        fixture
    }

    async fn sync(&mut self, epoch: u64, hash: Option<&str>) {
        self.generation += 1;
        let mut record = json!({"ref":"session", "scope_epoch":epoch, "kind":"head",
            "carriers":[{"principal":{"kind":"direct"}}]});
        if let Some(hash) = hash {
            record["bind_proof_sha256"] = json!(hash);
        }
        fs::write(
            self.root.join("sync.staging"),
            serde_json::to_vec(&json!([record])).unwrap(),
        )
        .unwrap();
        fs::rename(self.root.join("sync.staging"), self.root.join("sync.json")).unwrap();
        timeout(Duration::from_secs(10), async {
            loop {
                if let Some(reply) = events(self.root.join(format!("{OWNER}.jsonl")))
                    .into_iter()
                    .find(|e| {
                        e["corr"] == 1_000_000 + self.generation
                            && (e["kind"] == "scope_sync_response" || e["kind"] == "error")
                    })
                {
                    assert_eq!(reply["kind"], "scope_sync_response", "{reply}");
                    assert_ne!(
                        reply["body_json"]["results"][0]["outcome"], "refused",
                        "{reply}"
                    );
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("scope sync settles");
    }

    async fn open(&self, epoch: u64, proof: Option<&str>, owner: bool) -> Result<(), CallError> {
        let options = CallOptions {
            timeout: Duration::from_secs(5),
            route_retry_deadline: Duration::from_secs(3),
            consumer_identity: owner.then(|| ConsumerIdentity {
                module_id: OWNER.to_string(),
                launch_nonce: fs::read_to_string(self.root.join("owner.nonce")).unwrap(),
            }),
            ..Default::default()
        };
        self.consumer
            .open_route_scoped(
                RouteTarget::ToolProvider {
                    module_id: PROVIDER.to_string(),
                },
                BindIdentity::new("proof-test", "session"),
                ScopeSelector::new(
                    Principal::Reserved {
                        module_id: OWNER.to_string(),
                    },
                    "session",
                    Some(epoch),
                )
                .with_bind_proof(proof.map(str::to_string)),
                options,
            )
            .await
            .map(|_| ())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_bind_proof_requires_proof() {
    let mut f = Fixture::new().await;
    f.sync(1, Some(HASH)).await;
    let err = f.open(1, None, false).await.unwrap_err();
    assert_eq!(
        err.scope_bind_proof_refusal(),
        Some(ScopeBindProofRefusal::Required),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_bind_proof_rejects_wrong_proof() {
    let mut f = Fixture::new().await;
    f.sync(1, Some(HASH)).await;
    let err = f.open(1, Some(NEXT_PROOF), false).await.unwrap_err();
    assert_eq!(
        err.scope_bind_proof_refusal(),
        Some(ScopeBindProofRefusal::Mismatch),
        "{err:?}"
    );
    let refusal = serde_json::to_string(err.route_open_refusal().unwrap()).unwrap();
    assert!(!refusal.contains(NEXT_PROOF));
    assert!(!format!("{err:?} {err}").contains(NEXT_PROOF));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_bind_proof_rejects_malformed_proof() {
    let mut f = Fixture::new().await;
    f.sync(1, Some(HASH)).await;
    for proof in [
        "not+base64/",
        "",
        "Bwc=",
        "Bw",
        "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwd",
        "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=",
    ] {
        let err = f.open(1, Some(proof), false).await.unwrap_err();
        assert_eq!(
            err.scope_bind_proof_refusal(),
            Some(ScopeBindProofRefusal::Mismatch),
            "{err:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_bind_proof_admits_matching_digest() {
    let mut f = Fixture::new().await;
    f.sync(1, Some(HASH)).await;
    f.open(1, Some(PROOF), false).await.unwrap();
    f.open(1, Some(PROOF), true).await.unwrap();
    let attaches: Vec<_> = events(f.root.join(format!("{PROVIDER}.jsonl")))
        .into_iter()
        .filter(|e| e["kind"] == "attach")
        .collect();
    assert_eq!(attaches.len(), 2);
    let output = serde_json::to_string(&attaches).unwrap();
    assert!(!output.contains(PROOF));
    assert!(!output.contains(HASH));
    assert_eq!(attaches[0]["scope"]["ref"], "session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_bind_proof_reserved_owner_ignores_proof() {
    let mut f = Fixture::new().await;
    f.sync(1, Some(HASH)).await;
    for proof in [None, Some(NEXT_PROOF), Some("malformed+proof=")] {
        f.open(1, proof, true).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_bind_proof_absent_hash_ignores_proof() {
    let mut f = Fixture::new().await;
    f.sync(1, None).await;
    for proof in [None, Some(PROOF), Some("malformed+proof=")] {
        f.open(1, proof, false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_bind_proof_new_epoch_replaces_hash() {
    let mut f = Fixture::new().await;
    f.sync(1, Some(HASH)).await;
    f.open(1, Some(PROOF), false).await.unwrap();
    f.sync(2, Some(NEXT_HASH)).await;
    let err = f.open(2, Some(PROOF), false).await.unwrap_err();
    assert_eq!(
        err.scope_bind_proof_refusal(),
        Some(ScopeBindProofRefusal::Mismatch),
        "{err:?}"
    );
    f.open(2, Some(NEXT_PROOF), false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_bind_proof_daemon_logs_are_redacted() {
    let mut f = Fixture::new().await;
    f.sync(1, Some(HASH)).await;
    f.open(1, Some(NEXT_PROOF), false).await.unwrap_err();
    f.open(1, Some(PROOF), false).await.unwrap();
    let log = timeout(Duration::from_secs(5), async {
        loop {
            let log = fs::read_dir(f.root.join("data/cortexkit/run/logs"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter_map(|file| fs::read_to_string(file.path()).ok())
                .collect::<String>();
            if log.contains("route.open refused") && log.contains("scope_bind_proof_mismatch") {
                break log;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("proof refusal is captured in the daemon's log sink");
    f.consumer.close().await;
    f.child.kill().unwrap();
    f.child.wait().unwrap();
    assert!(!log.contains(PROOF));
    assert!(!log.contains(NEXT_PROOF));
}
