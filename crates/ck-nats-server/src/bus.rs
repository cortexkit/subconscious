use std::{path::PathBuf, sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::{json, Value};
use subc_client_rs::{
    CallOptions, ConsumerOptions, ProviderRole, RouteHandle, SubcConsumer, SubscribeOptions,
    Subscription,
};
use subc_protocol::{BindIdentity, RouteTarget};
use tokio::{
    sync::RwLock,
    time::{sleep, timeout},
};

use crate::{engine::Engine, wire::EngineMessage};

#[derive(Clone)]
pub(crate) struct BusRoute {
    consumer: Arc<SubcConsumer>,
    route: RouteHandle,
}

#[derive(Deserialize)]
struct Envelope<T> {
    result: T,
}
#[derive(Deserialize)]
struct LeafState {
    generation: u64,
    remotes: Vec<Value>,
}
#[derive(Deserialize)]
struct Signature {
    jwt: String,
    signature_b64: String,
}

fn target() -> RouteTarget {
    RouteTarget::ManagementSurface {
        module_id: "ckbus".into(),
    }
}
fn identity() -> BindIdentity {
    BindIdentity::new("ck-nats-server", "engine")
}

pub(crate) async fn sign(bus: Arc<RwLock<Option<BusRoute>>>, request: EngineMessage) -> Value {
    let (id, remote, nonce_b64) = match request {
        EngineMessage::Sign {
            id,
            remote,
            nonce_b64,
        } => (id, remote, nonce_b64),
        EngineMessage::TlsSign {
            id,
            message_b64,
            scheme,
        } => {
            // Hub TLS signing has no ck-bus operation yet, so every request answers unavailable.
            drop((message_b64, scheme));
            return json!({"type":"tls_sign_failed","id":id,"signer":"ck-bus","reason":"ck-bus unavailable"});
        }
        _ => unreachable!("only signing requests enter the signer"),
    };
    let route = bus.read().await.clone();
    let result = async {
        let route = route.ok_or_else(|| "ck-bus unavailable".to_string())?;
        let opts = CallOptions {
            timeout: Duration::from_millis(4_000),
            ..Default::default()
        };
        let reply = route.consumer.request(&route.route, serde_json::to_vec(&json!({"method":"ckbus.leaf_sign","params":{"remote":remote,"nonce_b64":nonce_b64}})).unwrap(), opts).await
            .map_err(|e| e.code().unwrap_or("ck-bus unavailable").to_owned())?;
        serde_json::from_slice::<Envelope<Signature>>(&reply)
            .map(|r| r.result)
            .map_err(|_| "ck-bus unavailable".to_owned())
    };
    match timeout(Duration::from_millis(4_000), result).await {
        Ok(Ok(signature)) => {
            json!({"type":"sign_ok","id":id,"jwt":signature.jwt,"signature_b64":signature.signature_b64})
        }
        other => {
            let reason = match other {
                Ok(Err(reason)) => reason,
                _ => "ck-bus unavailable".into(),
            };
            json!({"type":"sign_failed","id":id,"signer":"ck-bus","reason":reason})
        }
    }
}

pub(crate) async fn follow(
    connection: PathBuf,
    shared: Arc<RwLock<Option<BusRoute>>>,
    engine: Engine,
) {
    loop {
        if let Err(error) = follow_connection(&connection, &shared, &engine).await {
            eprintln!("ck-nats-server: ck-bus watch unavailable: {error}");
        }
        *shared.write().await = None;
        sleep(Duration::from_secs(1)).await;
    }
}

async fn follow_connection(
    connection: &std::path::Path,
    shared: &Arc<RwLock<Option<BusRoute>>>,
    engine: &Engine,
) -> Result<(), String> {
    let consumer = Arc::new(
        SubcConsumer::connect(connection, ConsumerOptions::default())
            .await
            .map_err(|e| e.to_string())?,
    );
    let result = follow_catalog(consumer.clone(), shared, engine).await;
    *shared.write().await = None;
    consumer.close().await;
    result
}

async fn follow_catalog(
    consumer: Arc<SubcConsumer>,
    shared: &Arc<RwLock<Option<BusRoute>>>,
    engine: &Engine,
) -> Result<(), String> {
    let mut catalog = consumer.catalog_watch().await.map_err(|e| e.to_string())?;
    let mut leaf_watch: Option<Subscription> = None;
    loop {
        tokio::select! {
            update = catalog.next() => {
                update.map_err(|e| e.to_string())?.ok_or("catalog.watch ended")?;
                let modules = consumer.catalog_list().await.map_err(|e| e.to_string())?;
                let available = modules.modules.iter().any(|module| module.module_id == "ckbus" && module.ready && module.roles.iter().any(|role| {
                    matches!(role, ProviderRole::ManagementSurface {operations,..} if operations.iter().any(|op| op.name == "ckbus.leaf_watch"))
                }));
                if !available {
                    leaf_watch = None;
                    *shared.write().await = None;
                    consumer.close_route(target(),identity(),Default::default()).await;
                } else if leaf_watch.is_none() {
                    let route = consumer.open_route(target(),identity(),CallOptions::default()).await.map_err(|e| e.to_string())?;
                    let subscription = consumer.subscribe_route(&route, serde_json::to_vec(&json!({"method":"ckbus.leaf_watch","params":{}})).unwrap(), SubscribeOptions::default()).await.map_err(|e| e.to_string())?;
                    *shared.write().await = Some(BusRoute {consumer:consumer.clone(),route});
                    leaf_watch = Some(subscription);
                }
            }
            event = async { leaf_watch.as_mut().unwrap().events().recv().await }, if leaf_watch.is_some() => {
                match event {
                    Some(bytes) => {
                        let state = serde_json::from_slice::<Envelope<LeafState>>(&bytes).map_err(|e| e.to_string())?.result;
                        engine.state(state.generation,state.remotes).await?;
                    }
                    None => return Err("ckbus.leaf_watch ended".into()),
                }
            }
        }
    }
}
