//! Exercise the production clients over a private daemon with scripted module replies.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{json, Value};
use subc_client_rs::{HandlerOutcome, ModuleHandler, RequestCtx};
use subc_daemon::bootstrap::{run_with_config, BootstrapConfig};
use subc_protocol::{
    manifest::{
        Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
    },
    ModuleHelloAckBody,
};
use tokio::{sync::Notify, task::JoinHandle};

use super::{tests::A, BoxId, OpenError, Opener, Peer, Peers};
use crate::{credentials::vault::ClaustrumRoute, federation::leaf::production::SubcFederation};

#[derive(Default)]
struct Replies {
    ready: Notify,
    open: Mutex<Value>,
    keys: Mutex<Value>,
    devices: Mutex<Value>,
}

struct Fixture(Arc<Replies>);

#[async_trait]
impl ModuleHandler for Fixture {
    async fn on_hello_ack(&self, _: &ModuleHelloAckBody) {
        self.0.ready.notify_one();
    }

    async fn handle(&self, _: RequestCtx, request: Vec<u8>) -> HandlerOutcome {
        let request: Value = serde_json::from_slice(&request).unwrap();
        let reply = match request["method"].as_str().unwrap() {
            "credential.open" => {
                assert_eq!(request["params"]["credential_id"], "kem:bus-seal:1");
                &self.0.open
            }
            "callosum.peer_keys_read" => &self.0.keys,
            "callosum.paired_devices_read" => &self.0.devices,
            other => panic!("unexpected request: {other}"),
        };
        HandlerOutcome::Response(serde_json::to_vec(&*reply.lock().unwrap()).unwrap())
    }
}

async fn endpoint(
    path: &Path,
    module: &str,
    operations: &[&str],
) -> (Arc<Replies>, JoinHandle<()>) {
    let manifest = ModuleManifest::builder(module, "1.0.0")
        .provides(vec![ProviderRole::ManagementSurface {
            operations: operations
                .iter()
                .map(|op| ManagementOperation {
                    name: (*op).into(),
                    kind: ManagementOperationKind::Query,
                    description: None,
                })
                .collect(),
            config_schema: json!({}),
            observability: vec![],
            identity_scope: vec![],
            concurrency: Concurrency::ModuleManaged,
        }])
        .build();
    let replies = Arc::new(Replies::default());
    let fixture = Fixture(replies.clone());
    let path = path.to_path_buf();
    let task = tokio::spawn(async move {
        subc_client_rs::serve_with(&path, manifest, fixture)
            .await
            .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(10), replies.ready.notified())
        .await
        .unwrap();
    (replies, task)
}

#[tokio::test]
async fn handoff_production_routes_classify_vault_and_peer_replies() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("connection.json");
    let daemon = tokio::spawn(run_with_config(BootstrapConfig::new(&path, 0)));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !path.is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let vault = ClaustrumRoute::new(path.clone(), None);
    let open = || vault.open(&[0; 32], &[0; 16], b"ck-bus-seal/v1/ckbus/fixture");
    assert!(matches!(open().await, Err(OpenError::Unavailable(_))));
    let (claustrum, vault_task) = endpoint(&path, "claustrum", &["credential.open"]).await;
    for (code, class) in [
        ("vault_locked", "transient"),
        ("refresh_failed", "transient"),
        ("not_found", "permanent"),
        ("kind_not_openable", "permanent"),
        ("open_failed", "transient"),
    ] {
        *claustrum.open.lock().unwrap() = json!({"result":{"error":{"code":code,"class":class}}});
        assert!(
            matches!(open().await, Err(OpenError::Unavailable(_))),
            "{code} ({class})"
        );
    }
    *claustrum.open.lock().unwrap() =
        json!({"result":{"error":{"code":"open_rate_limited","class":"transient"}}});
    assert!(matches!(open().await, Err(OpenError::RateLimited)));
    *claustrum.open.lock().unwrap() =
        json!({"result":{"error":{"code":"open_failed","class":"permanent"}}});
    assert!(matches!(open().await, Err(OpenError::Refused(code)) if code == "open_failed"));
    *claustrum.open.lock().unwrap() = json!({"result":{"plaintext_b64":"AA==","key_id":"fixture"}});
    assert_eq!(open().await.unwrap(), [0]);

    let (callosum, peer_task) = endpoint(
        &path,
        "callosum",
        &["callosum.peer_keys_read", "callosum.paired_devices_read"],
    )
    .await;
    let roster = SubcFederation::new(path);
    let a = BoxId::parse(A).unwrap();
    let record = json!({"machine_id": A, "fingerprint":"fingerprint", "key_record":{"keys": {
        "bus_seal_x25519":{"pub":"ab".repeat(32)}, "msgsig_ed25519":{"pub":"cd".repeat(32)}
    }}});
    *callosum.keys.lock().unwrap() = json!({"peers":[]});
    assert!(matches!(roster.lookup(&a).await.unwrap(), Peer::Unpaired));
    *callosum.keys.lock().unwrap() = json!({"peers":[record, record]});
    assert!(matches!(roster.lookup(&a).await.unwrap(), Peer::Ambiguous));
    *callosum.keys.lock().unwrap() = json!({"peers":[record]});
    let devices = json!([{"fingerprint":"fingerprint","class":"computer"}]);
    *callosum.devices.lock().unwrap() =
        json!({"devices":devices,"tombstones":[{"fingerprint":"fingerprint"}]});
    assert!(matches!(roster.lookup(&a).await.unwrap(), Peer::Unpaired));
    *callosum.devices.lock().unwrap() = json!({"devices":devices,"tombstones":[]});
    let mut invalid = record.clone();
    invalid["key_record"]["keys"]["msgsig_ed25519"]["pub"] = json!("malformed");
    *callosum.keys.lock().unwrap() = json!({"peers":[invalid]});
    assert!(matches!(roster.lookup(&a).await.unwrap(), Peer::MissingKey));
    *callosum.keys.lock().unwrap() = json!({"peers":[record]});
    assert!(
        matches!(roster.lookup(&a).await.unwrap(), Peer::Live(keys) if keys.seal == [0xab; 32] && keys.sign == [0xcd; 32])
    );

    for task in [vault_task, peer_task] {
        task.abort();
        let _ = task.await;
    }
    daemon.abort();
    let _ = daemon.await;
}
