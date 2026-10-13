#![forbid(unsafe_code)]

//! A supervised activity reporter for the SDK's real-daemon tests. It checks read
//! authorization from a separate consumer connection, records an initial event,
//! then records another when the file named by SUBC_ACTIVITY_TRIGGER appears.

use std::{error::Error, fs, path::PathBuf, time::Duration};

use serde_json::json;
use subc_client_rs::{
    async_trait, ActivityQueryRequest, ActivityRecordRequest, ConsumerOptions, HandlerOutcome,
    ModuleHandler, RequestCtx, SubcConsumer,
};
use subc_protocol::manifest::ModuleManifest;

struct IdleHandler;
#[async_trait]
impl ModuleHandler for IdleHandler {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(b"{}".to_vec())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let _ = subc_client_rs::launch_nonce();
    let module_id = std::env::var(subc_protocol::SUBC_MODULE_ID_ENV)?;
    let connection_file = std::env::args_os()
        .skip_while(|arg| arg != "--subc")
        .nth(1)
        .map(PathBuf::from)
        .ok_or("missing --subc")?;
    let results =
        PathBuf::from(std::env::var_os("SUBC_ACTIVITY_RESULTS").ok_or("missing results path")?);
    let trigger =
        PathBuf::from(std::env::var_os("SUBC_ACTIVITY_TRIGGER").ok_or("missing trigger path")?);
    let manifest = ModuleManifest::builder(&module_id, env!("CARGO_PKG_VERSION")).build();
    let (handle, serving) =
        subc_client_rs::serve_with_handle(&connection_file, manifest, IdleHandler).await?;
    let serving = tokio::spawn(serving);
    let consumer = SubcConsumer::connect(&connection_file, ConsumerOptions::default()).await?;
    let query =
        serde_json::from_value::<ActivityQueryRequest>(json!({"op":"activity.query", "limit":10}))?;
    let query_code = consumer
        .query_activity(query)
        .await
        .err()
        .and_then(|err| err.code().map(str::to_owned));
    let mut subscription = consumer.subscribe_activity(None).await?;
    let subscribe_code =
        match tokio::time::timeout(Duration::from_secs(5), subscription.next()).await {
            Ok(Err(err)) => err.code().map(str::to_owned),
            other => Some(format!("unexpected subscribe result: {other:?}")),
        };
    let initial = handle.record_activity(event(&module_id, "initial")).await?;
    fs::write(
        &results,
        serde_json::to_vec(&json!({
            "query_code": query_code, "subscribe_code": subscribe_code, "seq": initial.seq,
        }))?,
    )?;
    while !trigger.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.record_activity(event(&module_id, "live")).await?;
    consumer.close().await;
    serving.await??;
    Ok(())
}

fn event(module_id: &str, summary: &str) -> ActivityRecordRequest {
    ActivityRecordRequest {
        kind: format!("{module_id}.changed"),
        summary: summary.to_string(),
        subject: None,
        details: None,
        on: None,
    }
}
