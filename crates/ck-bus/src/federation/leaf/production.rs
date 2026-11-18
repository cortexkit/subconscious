//! The leaf area's production seams: callosum and the hub's ck-bus over ck-bus's own subc
//! client connection, and the local server through bootstrap's system-account plane.

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::{json, Value};
use subc_client_rs::consumer::{
    CallError, CallOptions, ConsumerOptions, SubcConsumer, SubscribeOptions,
};
use subc_protocol::{BindIdentity, RouteTarget};
use tokio::sync::{mpsc, watch, OnceCell};

use super::seams::{Callosum, FederationAccounts, LeafLinks, LocalHub, PeerIssuer};
use crate::{
    bootstrap::federation_account::ensure_federation_account,
    bootstrap::{plane::LeafLink, store::Store, Ready},
    credentials::Credentials,
};

pub const CALLOSUM_MODULE_ID: &str = "callosum";

/// The module id callosum gives a paired computer's re-exported ck-bus ops.
pub fn peer_ckbus_module(hub_fingerprint: &str) -> String {
    format!("fed:{hub_fingerprint}:ckbus")
}

/// How long a call to callosum or through it may take. The bridge waits for the hub
/// computer to answer `bridge_ready`, and an issue crosses to the hub and its vault.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Callosum's ops and the hub's re-exported `hub_leaf_issue`, over one client connection
/// to the daemon that connects on first use.
pub struct SubcFederation {
    connection_file: PathBuf,
    pub(crate) bind: BindIdentity,
    consumer: OnceCell<SubcConsumer>,
}

impl SubcFederation {
    pub fn new(connection_file: PathBuf) -> Self {
        Self {
            connection_file,
            bind: BindIdentity::new("ck-bus", "federation"),
            consumer: OnceCell::new(),
        }
    }

    pub(crate) async fn consumer(&self) -> Result<&SubcConsumer, String> {
        self.consumer
            .get_or_try_init(|| async {
                SubcConsumer::connect(&self.connection_file, ConsumerOptions::default()).await
            })
            .await
            .map_err(|error| format!("cannot reach the daemon: {error}"))
    }

    pub(crate) async fn call(
        &self,
        module_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let consumer = self.consumer().await?;
        let reply = consumer
            .call(
                RouteTarget::ManagementSurface {
                    module_id: module_id.to_string(),
                },
                self.bind.clone(),
                body(method, params),
                CallOptions {
                    timeout: CALL_TIMEOUT,
                    ..CallOptions::default()
                },
            )
            .await
            .map_err(|error| describe(module_id, method, &error))?;
        let value: Value = serde_json::from_slice(&reply)
            .map_err(|error| format!("{method} reply is not JSON: {error}"))?;
        Ok(unwrap_result(value))
    }
}

pub(crate) fn body(method: &str, params: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({ "method": method, "params": params })).expect("a request encodes")
}

pub(crate) fn describe(module_id: &str, method: &str, error: &CallError) -> String {
    let code = error
        .route_open_refusal()
        .map(|refusal| refusal.code.clone())
        .or_else(|| error.code().map(str::to_string));
    match code {
        Some(code) => format!("{method} on {module_id}: {code}: {error}"),
        None => format!("{method} on {module_id}: {error}"),
    }
}

/// ck-bus answers inside `{"result": ...}`; callosum's local ops answer the body itself.
/// Either reads the same.
fn unwrap_result(value: Value) -> Value {
    match value {
        Value::Object(mut object) if object.len() == 1 && object.contains_key("result") => {
            object.remove("result").unwrap_or(Value::Null)
        }
        other => other,
    }
}

#[async_trait]
impl Callosum for SubcFederation {
    async fn hub_watch(&self) -> Result<mpsc::Receiver<Vec<u8>>, String> {
        let consumer = self.consumer().await?;
        let method = "callosum.hub_watch";
        let mut subscription = consumer
            .subscribe(
                RouteTarget::ManagementSurface {
                    module_id: CALLOSUM_MODULE_ID.to_string(),
                },
                self.bind.clone(),
                body(method, json!({})),
                SubscribeOptions::default(),
            )
            .await
            .map_err(|error| describe(CALLOSUM_MODULE_ID, method, &error))?;
        let (notices, receiver) = mpsc::channel(16);
        tokio::spawn(async move {
            while let Some(notice) = subscription.events().recv().await {
                if notices.send(notice).await.is_err() {
                    return;
                }
            }
        });
        Ok(receiver)
    }

    async fn hub_read(&self) -> Result<Vec<u8>, String> {
        let value = self
            .call(CALLOSUM_MODULE_ID, "callosum.hub_read", json!({}))
            .await?;
        Ok(serde_json::to_vec(&value).expect("a JSON value encodes"))
    }

    async fn leaf_bridge_open(&self, fingerprint: &str) -> Result<u16, String> {
        let value = self
            .call(
                CALLOSUM_MODULE_ID,
                "callosum.leaf_bridge_open",
                json!({ "fingerprint": fingerprint }),
            )
            .await?;
        value["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("callosum.leaf_bridge_open answered no port: {value}"))
    }
}

#[async_trait]
impl PeerIssuer for SubcFederation {
    async fn hub_leaf_issue(
        &self,
        hub_fingerprint: &str,
        leaf_nkey: &str,
    ) -> Result<String, String> {
        let value = self
            .call(
                &peer_ckbus_module(hub_fingerprint),
                "ckbus.hub_leaf_issue",
                json!({ "leaf_nkey": leaf_nkey }),
            )
            .await?;
        value["jwt"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "ckbus.hub_leaf_issue answered no jwt".to_string())
    }
}

fn ready(ready: &watch::Receiver<Option<Arc<Ready>>>) -> Result<Arc<Ready>, String> {
    ready
        .borrow()
        .clone()
        .ok_or_else(|| "bootstrap has not finished".to_string())
}

/// The leaf report and the kick through bootstrap's system-account connection.
pub struct SystemLeafLinks {
    pub ready: watch::Receiver<Option<Arc<Ready>>>,
}

#[async_trait]
impl LeafLinks for SystemLeafLinks {
    async fn leafs(&self, account: &str) -> Result<Vec<LeafLink>, String> {
        ready(&self.ready)?
            .system
            .leafz(account)
            .await
            .map_err(|error| error.message)
    }

    async fn kick(&self, link: &LeafLink) -> Result<(), String> {
        ready(&self.ready)?
            .system
            .kick(&link.server_id, link.id)
            .await
            .map_err(|error| error.message)
    }
}

/// The federation account, created through bootstrap's system-account connection once
/// bootstrap is ready.
pub struct ReadyFederationAccounts {
    pub ready: watch::Receiver<Option<Arc<Ready>>>,
    pub credentials: Arc<Credentials>,
    pub store: Store,
}

#[async_trait]
impl FederationAccounts for ReadyFederationAccounts {
    async fn ensure(&self) -> Result<String, String> {
        let ready = ready(&self.ready)?;
        if let Some(account) = ready.fedout_account.get() {
            return Ok(account.clone());
        }
        ensure_federation_account(
            ready.system.as_ref(),
            &self.credentials,
            &self.store,
            &ready.account.machine_id,
        )
        .await
    }
}

/// Starts the leaf area with its production seams. Nothing is dialled until callosum
/// names a hub; bootstrap must be ready before the federation account or the leaf report
/// is touched. `local_hub` is this computer's own hub, for when it is the hub.
pub fn start(
    federation: Arc<SubcFederation>,
    local_hub: Arc<dyn LocalHub>,
    credentials: Arc<Credentials>,
    store_root: &std::path::Path,
    ready: watch::Receiver<Option<Arc<Ready>>>,
) -> Arc<super::area::LeafArea> {
    let area = super::area::LeafArea::new(
        super::area::LeafDeps {
            callosum: federation.clone(),
            issuer: federation,
            local_hub,
            vault: credentials.vault.clone(),
            links: Arc::new(SystemLeafLinks {
                ready: ready.clone(),
            }),
            quiet: Arc::new(super::seams::AlwaysQuiet),
            accounts: Arc::new(ReadyFederationAccounts {
                ready,
                credentials,
                store: Store::new(store_root.to_path_buf()),
            }),
        },
        super::area::Timing::default(),
    );
    area.spawn();
    area
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_reply_framing_reads_the_same() {
        assert_eq!(
            unwrap_result(json!({"result": {"port": 49152}})),
            json!({"port": 49152})
        );
        // `callosum/test-vectors/leaf-bridge-v1/ops.json`, the reply.
        assert_eq!(
            unwrap_result(json!({"port": 49152})),
            json!({"port": 49152})
        );
        assert_eq!(
            peer_ckbus_module("0123456789abcdef0123456789abcdef"),
            "fed:0123456789abcdef0123456789abcdef:ckbus"
        );
    }
}
