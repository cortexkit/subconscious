#![cfg(unix)]

mod common;

use common::connect_authed_client;
use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::{json, Value};
use std::{
    fs,
    process::{Child, Stdio},
    time::Duration,
};
use subc_client_rs::{CallOptions, SubcConsumer};
use subc_control::ClientControlResponse;
use subc_daemon::{read_frame, write_frame};
use subc_protocol::{
    manifest::{Concurrency, IdentityScope, ModuleManifest, ProviderRole},
    scope::ScopeSelector,
    BindIdentity, Flags, Frame, FrameType, ModuleHelloBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use tokio::time::{sleep, timeout};
use tokio::{io::AsyncWriteExt, net::TcpStream};

const OWNER: &str = "served-owner";
const PROVIDER: &str = "served-provider";
const REEXPORT: &str = "reexport:peer:tool";

async fn read_frame_timeout(stream: &mut TcpStream) -> Frame {
    timeout(Duration::from_secs(10), read_frame(stream))
        .await
        .expect("timed out waiting for frame")
        .unwrap()
        .expect("connection should stay open")
}

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
        Self::new_with_federated_owner(provider_env, false).await
    }

    async fn new_with_federated_owner(provider_env: Value, federated: bool) -> Self {
        Self::new_with_policy(provider_env, federated, false).await
    }

    async fn new_with_prefix() -> Self {
        Self::new_with_policy(json!({}), false, true).await
    }

    async fn new_with_policy(provider_env: Value, federated: bool, prefix: bool) -> Self {
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
        let mut config = json!({
            "version":1, "modules":{OWNER:module(OWNER), PROVIDER:module(PROVIDER)}
        });
        if federated {
            config["federated_scope_owner"] = json!(OWNER);
        }
        if prefix {
            config["modules"][PROVIDER]["reserved"] = json!(true);
            config["modules"][PROVIDER]["reserved_prefixes"] = json!(["reexport:"]);
            config["modules"][PROVIDER]["env"]["FAKE_AFT_LAUNCH_NONCE_PATH"] =
                json!(root.join("prefix-owner.nonce"));
        }
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&config).unwrap(),
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
        .unwrap_or_else(|error| {
            panic!(
                "isolated daemon and modules register: {error}; {}",
                fs::read_to_string(root.join("daemon.log")).unwrap_or_default()
            )
        })
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
        self.open_target(epoch, PROVIDER, true).await
    }

    async fn open_target(
        &self,
        epoch: u64,
        module: &str,
        wait_for_detach: bool,
    ) -> Result<(), subc_client_rs::CallError> {
        let handle = self
            .consumer
            .open_route_scoped(
                RouteTarget::ToolProvider {
                    module_id: module.into(),
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
        if wait_for_detach {
            timeout(Duration::from_secs(5), async {
                while !self.saw_module_event(module, "detach") {
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("target observes route closure before history lookup");
        }
        Ok(())
    }

    fn saw_bind(&self, kind: &str) -> bool {
        self.saw_module_event(PROVIDER, kind)
    }

    fn saw_module_event(&self, module: &str, kind: &str) -> bool {
        fs::read_to_string(self.root.join(format!("{module}.jsonl")))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["kind"] == kind)
    }

    async fn prefix_nonce(&self) -> String {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(nonce) = fs::read_to_string(self.root.join("prefix-owner.nonce")) {
                    return nonce;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("supervised prefix owner writes its real launch nonce")
    }

    async fn prefix_hello(&self, nonce: Option<String>) -> (TcpStream, Frame) {
        let mut stream = connect_authed_client(
            self.root
                .join("runtime")
                .join(subc_transport::CONNECTION_FILE_NAME),
        )
        .await
        .unwrap();
        let mut manifest = ModuleManifest::builder(REEXPORT, "0.0.0-test").build();
        manifest.provides = vec![ProviderRole::ToolProvider {
            tools: vec![],
            identity_scope: vec![IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: false,
            sub_supervises: false,
        }];
        let hello = Frame::build(
            FrameType::Hello,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&ModuleHelloBody {
                manifest,
                protocol_ver: PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: nonce,
            })
            .unwrap(),
        )
        .unwrap();
        write_frame(&mut stream, &hello).await.unwrap();
        stream.flush().await.unwrap();
        let reply = read_frame_timeout(&mut stream).await;
        (stream, reply)
    }

    async fn bind_reexport(
        &self,
        stream: &mut TcpStream,
        reply: Option<FrameType>,
    ) -> Result<(), subc_client_rs::CallError> {
        let acknowledge = async {
            let bind = read_frame_timeout(stream).await;
            assert_eq!(bind.header.ty, FrameType::Request);
            let body: Value = serde_json::from_slice(&bind.body).unwrap();
            assert_eq!(body["op"], "route.bind");
            assert_eq!(body["scope"]["scope_epoch"], 1);
            if let Some(ty) = reply {
                let body = if ty == FrameType::Response {
                    json!({"op":"route.bind"})
                } else {
                    json!({"code":"bind_refused", "message":"test refusal"})
                };
                let response = Frame::build(
                    ty,
                    bind.header.flags,
                    0,
                    0,
                    bind.header.corr,
                    serde_json::to_vec(&body).unwrap(),
                )
                .unwrap();
                write_frame(stream, &response).await.unwrap();
                stream.flush().await.unwrap();
            }
        };
        let (opened, ()) = tokio::join!(self.open_target(1, REEXPORT, false), acknowledge);
        opened
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_by_verified_prefix_credits_registration_and_owner() {
    let mut f = Fixture::new_with_prefix().await;
    f.sync(Some(1)).await;
    let (mut stream, hello) = f.prefix_hello(Some(f.prefix_nonce().await)).await;
    assert_eq!(hello.header.ty, FrameType::HelloAck);
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "not_served");
    f.bind_reexport(&mut stream, Some(FrameType::Response))
        .await
        .unwrap();
    drop(stream);
    assert_eq!(f.served(OWNER, 1, REEXPORT).await["answer"], "served");
    assert_eq!(f.served(OWNER, 1, OWNER).await["answer"], "not_served");
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "served");
    f.sync(Some(1)).await;
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "served");
    f.sync(Some(2)).await;
    for module in [REEXPORT, PROVIDER] {
        assert_eq!(f.served(OWNER, 2, module).await["answer"], "not_served");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_by_prefix_without_owner_nonce_is_refused() {
    let mut f = Fixture::new_with_prefix().await;
    f.sync(Some(1)).await;
    for nonce in [None, Some("not-the-owner-nonce".to_string())] {
        let (_stream, hello) = f.prefix_hello(nonce).await;
        assert_eq!(hello.header.ty, FrameType::Error);
        let error: Value = serde_json::from_slice(&hello.body).unwrap();
        assert_eq!(error["code"], "reserved_module");
    }
    assert!(!f
        .consumer
        .catalog_list()
        .await
        .unwrap()
        .modules
        .iter()
        .any(|m| m.module_id == REEXPORT));
    for module in [REEXPORT, PROVIDER] {
        assert_eq!(f.served(OWNER, 1, module).await["answer"], "not_served");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_by_prefix_policy_added_after_hello_credits_no_owner() {
    let mut f = Fixture::new(json!({})).await;
    f.sync(Some(1)).await;
    let (mut stream, hello) = f.prefix_hello(None).await;
    assert_eq!(hello.header.ty, FrameType::HelloAck);

    // Installing a policy later must not retroactively authenticate this HELLO.
    let config_path = f.root.join("config/cortexkit/subc.jsonc");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["modules"][PROVIDER]["reserved"] = json!(true);
    config["modules"][PROVIDER]["reserved_prefixes"] = json!(["reexport:"]);
    fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut control = connect_authed_client(
        f.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME),
    )
    .await
    .unwrap();
    let rescan = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        2,
        serde_json::to_vec(&json!({"op":"supervisor.rescan", "preview":false})).unwrap(),
    )
    .unwrap();
    write_frame(&mut control, &rescan).await.unwrap();
    control.flush().await.unwrap();
    let response = read_frame_timeout(&mut control).await;
    assert_eq!(response.header.ty, FrameType::Response);
    let ClientControlResponse::SupervisorRescan { result } =
        serde_json::from_slice(&response.body).unwrap()
    else {
        panic!("expected supervisor.rescan response");
    };
    assert_eq!(result.changed_pending_reload, [PROVIDER]);
    let (_foreign, refused) = f.prefix_hello(None).await;
    assert_eq!(refused.header.ty, FrameType::Error);
    let error: Value = serde_json::from_slice(&refused.body).unwrap();
    assert_eq!(
        error["code"], "reserved_module",
        "policy must apply to new HELLOs"
    );

    f.bind_reexport(&mut stream, Some(FrameType::Response))
        .await
        .unwrap();
    assert_eq!(f.served(OWNER, 1, REEXPORT).await["answer"], "served");
    assert_eq!(f.served(OWNER, 1, PROVIDER).await["answer"], "not_served");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_by_verified_prefix_refused_or_unacknowledged_bind_credits_neither() {
    for reply in [Some(FrameType::Error), None] {
        let mut f = Fixture::new_with_prefix().await;
        f.sync(Some(1)).await;
        let (mut stream, hello) = f.prefix_hello(Some(f.prefix_nonce().await)).await;
        assert_eq!(hello.header.ty, FrameType::HelloAck);
        assert!(f.bind_reexport(&mut stream, reply).await.is_err());
        for module in [REEXPORT, PROVIDER] {
            assert_eq!(f.served(OWNER, 1, module).await["answer"], "not_served");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn federated_describe_survives_real_daemon_restart_without_serving_history() {
    let mut f = Fixture::new_with_federated_owner(
        json!({
            "FAKE_AFT_CAPABILITIES": json!({"provides":["scope-federated/v1"]}).to_string()
        }),
        true,
    )
    .await;
    let describe = json!({"op":"scope.describe", "owner":{"kind":"reserved","module_id":OWNER}, "ref":"session"});
    let initial = f.request(PROVIDER, describe.clone()).await;
    assert_eq!(initial["status"], "not_live");
    assert_eq!(initial["owner_synced"], false);
    assert_eq!(initial["owner_configured"], true);
    let record = json!({"ref":"session", "scope_epoch":1, "kind":"federated", "attributes":{"federated":{
        "peer_class":"computer", "peer_fingerprint":"a".repeat(32), "pairing_id":"pair:1", "peer_name":"Peer"
    }}});
    let synced = f
        .request(
            OWNER,
            json!({"op":"scope.sync", "generation":1, "scopes":[record.clone()]}),
        )
        .await;
    assert_eq!(synced["results"][0]["outcome"], "created");
    let before = f.request(PROVIDER, describe.clone()).await;
    assert_eq!(before["status"], "live");
    assert_eq!(before["scope"]["kind"], "federated");
    assert_eq!(before["scope"]["owner_authorized"], false);
    f.child.kill().unwrap();
    f.child.wait().unwrap();
    for id in [OWNER, PROVIDER] {
        let _ = fs::remove_file(f.root.join(format!("{id}.request")));
    }
    f.child = spawn(&f.root);
    f.consumer = Fixture::connect(&f.root).await;
    let after = f.request(PROVIDER, describe.clone()).await;
    assert_ne!(before["daemon_incarnation"], after["daemon_incarnation"]);
    assert_eq!(after["status"], "not_live");
    assert_eq!(after["owner_synced"], false);
    assert_eq!(after["owner_configured"], true);
    let synced = f
        .request(
            OWNER,
            json!({"op":"scope.sync", "generation":2, "scopes":[record]}),
        )
        .await;
    assert_eq!(synced["results"][0]["outcome"], "created");
    let live = f.request(PROVIDER, describe).await;
    assert_eq!(live["status"], "live");
    assert_eq!(live["scope"], before["scope"]);
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
