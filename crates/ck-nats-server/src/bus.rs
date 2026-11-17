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
struct HubState {
    generation: u64,
    hub_listener: Option<HubListener>,
}
#[derive(Clone, serde::Serialize, Deserialize)]
pub(crate) struct HubListener {
    port: u16,
    cert_der_b64: String,
}
#[derive(Deserialize)]
struct Signature {
    jwt: Option<String>,
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
    let (id, method, params, tls) = match request {
        EngineMessage::Sign {
            id,
            remote,
            nonce_b64,
        } => (
            id,
            "ckbus.leaf_sign",
            json!({"remote":remote,"nonce_b64":nonce_b64}),
            false,
        ),
        EngineMessage::TlsSign {
            id,
            message_b64,
            scheme,
        } => (
            id,
            "ckbus.hub_tls_sign",
            json!({"message_b64":message_b64,"scheme":scheme}),
            true,
        ),
        _ => unreachable!("only signing requests enter the signer"),
    };
    let route = bus.read().await.clone();
    let result = async {
        let route = route.ok_or_else(|| "ck-bus unavailable".to_string())?;
        let opts = CallOptions {
            timeout: Duration::from_millis(4_000),
            ..Default::default()
        };
        let reply = route
            .consumer
            .request(
                &route.route,
                serde_json::to_vec(&json!({"method":method,"params":params})).unwrap(),
                opts,
            )
            .await
            .map_err(|e| e.code().unwrap_or("ck-bus unavailable").to_owned())?;
        serde_json::from_slice::<Envelope<Signature>>(&reply)
            .map(|r| r.result)
            .and_then(|signature| {
                if tls || signature.jwt.is_some() {
                    Ok(signature)
                } else {
                    Err(serde::de::Error::custom("missing jwt"))
                }
            })
            .map_err(|_| "ck-bus unavailable".to_owned())
    };
    match timeout(Duration::from_millis(4_000), result).await {
        Ok(Ok(signature)) if tls => {
            json!({"type":"tls_sign_ok","id":id,"signature_b64":signature.signature_b64})
        }
        Ok(Ok(signature)) => {
            json!({"type":"sign_ok","id":id,"jwt":signature.jwt,"signature_b64":signature.signature_b64})
        }
        other => {
            let reason = match other {
                Ok(Err(reason)) => reason,
                _ => "ck-bus unavailable".into(),
            };
            json!({"type":if tls { "tls_sign_failed" } else { "sign_failed" },"id":id,"signer":"ck-bus","reason":reason})
        }
    }
}

pub(crate) async fn follow(
    connection: PathBuf,
    shared: Arc<RwLock<Option<BusRoute>>>,
    engine: Engine,
    hub: bool,
) {
    loop {
        if let Err(error) = follow_connection(&connection, &shared, &engine, hub).await {
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
    hub: bool,
) -> Result<(), String> {
    let consumer = Arc::new(
        SubcConsumer::connect(connection, ConsumerOptions::default())
            .await
            .map_err(|e| e.to_string())?,
    );
    let result = follow_catalog(consumer.clone(), shared, engine, hub).await;
    *shared.write().await = None;
    consumer.close().await;
    result
}

async fn follow_catalog(
    consumer: Arc<SubcConsumer>,
    shared: &Arc<RwLock<Option<BusRoute>>>,
    engine: &Engine,
    hub: bool,
) -> Result<(), String> {
    let watch_method = if hub {
        "ckbus.hub_server_watch"
    } else {
        "ckbus.leaf_watch"
    };
    let mut catalog = consumer.catalog_watch().await.map_err(|e| e.to_string())?;
    let mut leaf_watch: Option<Subscription> = None;
    loop {
        tokio::select! {
            update = catalog.next() => {
                update.map_err(|e| e.to_string())?.ok_or("catalog.watch ended")?;
                let modules = consumer.catalog_list().await.map_err(|e| e.to_string())?;
                let available = modules.modules.iter().any(|module| module.module_id == "ckbus" && module.ready && module.roles.iter().any(|role| {
                    matches!(role, ProviderRole::ManagementSurface {operations,..} if operations.iter().any(|op| op.name == watch_method))
                }));
                if !available {
                    leaf_watch = None;
                    *shared.write().await = None;
                    consumer.close_route(target(),identity(),Default::default()).await;
                } else if leaf_watch.is_none() {
                    let route = consumer.open_route(target(),identity(),CallOptions::default()).await.map_err(|e| e.to_string())?;
                    let subscription = consumer.subscribe_route(&route, serde_json::to_vec(&json!({"method":watch_method,"params":{}})).unwrap(), SubscribeOptions::default()).await.map_err(|e| e.to_string())?;
                    *shared.write().await = Some(BusRoute {consumer:consumer.clone(),route});
                    leaf_watch = Some(subscription);
                }
            }
            event = async { leaf_watch.as_mut().unwrap().events().recv().await }, if leaf_watch.is_some() => {
                match event {
                    Some(bytes) => {
                        if hub {
                            let state = serde_json::from_slice::<Envelope<HubState>>(&bytes).map_err(|e| e.to_string())?.result;
                            engine.state(state.generation, vec![], state.hub_listener).await?;
                        } else {
                            let state = serde_json::from_slice::<Envelope<LeafState>>(&bytes).map_err(|e| e.to_string())?.result;
                            engine.state(state.generation,state.remotes,None).await?;
                        }
                    }
                    None => return Err(format!("{watch_method} ended")),
                }
            }
        }
    }
}
