use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use subc_client_rs::{CallOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

use crate::models::{
    AskRequest, BoardState, BoardSummary, ConsultDetail, ConsultRow, Snapshot, SpecCampaign,
};

const CONNECTION_FILE: &str = ".local/share/cortexkit/run/subc-connection.json";

pub fn load_live_blocking() -> Result<Snapshot> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(12), load_live())
            .await
            .map_err(|_| anyhow!("live snapshot deadline elapsed"))?
    });
    runtime.shutdown_background();
    result
}

async fn load_live() -> Result<Snapshot> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let connection_file = PathBuf::from(home).join(CONNECTION_FILE);
    let caller_directory = std::env::current_dir()?.canonicalize()?;
    let session_id = format!("gpui-spike-{}", uuid::Uuid::new_v4());
    let consumer = SubcConsumer::connect(&connection_file, ConsumerOptions::default()).await?;
    let target = RouteTarget::ManagementSurface {
        module_id: "alfonso-core".into(),
    };
    let identity = BindIdentity {
        project_root: caller_directory.clone(),
        harness: "ck-app".into(),
        session: session_id.clone(),
    };

    let call = |method: &'static str, params: Value| {
        call(
            &consumer,
            target.clone(),
            identity.clone(),
            caller_directory.clone(),
            session_id.clone(),
            method,
            params,
        )
    };
    let boards_value = call("board.list", json!({})).await?;
    let boards: Vec<BoardSummary> = decode_rows(boards_value, "boards")?;
    let asks_value = call("ask.list_pending_for_user", json!({})).await?;
    let asks: Vec<AskRequest> = decode_rows(asks_value, "asks")?;
    let consults_value = call("athena.list_consults", json!({"limit": 50})).await?;
    let consults: Vec<ConsultRow> = decode_rows(consults_value, "consults")?;
    let campaigns_value = call("athena.spec_status", json!({})).await?;
    let campaigns: Vec<SpecCampaign> = decode_rows(campaigns_value, "consults")?;

    let board = if let Some(summary) = boards.first() {
        let result = call(
            "board.state",
            json!({"harness": summary.harness, "session": summary.session}),
        )
        .await?;
        Some(serde_json::from_value::<BoardState>(result)?.fold_newest())
    } else {
        None
    };
    let consult_detail = if let Some(row) = consults.first() {
        let result = call("athena.get_consult", json!({"consultId": row.consult_id})).await?;
        Some(serde_json::from_value::<ConsultDetail>(result)?)
    } else {
        None
    };
    consumer.close().await;
    Ok(Snapshot {
        boards,
        board,
        asks,
        consults,
        consult_detail,
        campaigns,
    })
}

async fn call(
    consumer: &SubcConsumer,
    target: RouteTarget,
    identity: BindIdentity,
    caller_directory: PathBuf,
    session_id: String,
    method: &str,
    params: Value,
) -> Result<Value> {
    let mut params = params
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("params must be an object"))?;
    params.entry("harness").or_insert(json!("ck-app"));
    params
        .entry("sessionId")
        .or_insert(json!(session_id.clone()));
    params.entry("session").or_insert(json!(session_id));
    params
        .entry("callerDirectory")
        .or_insert(json!(caller_directory));
    let body = serde_json::to_vec(&json!({"method": method, "params": params}))?;
    let opts = CallOptions {
        timeout: Duration::from_secs(5),
        route_retry_deadline: Duration::from_secs(4),
        ..Default::default()
    };
    let bytes = consumer
        .call(target, identity, body, opts)
        .await
        .with_context(|| format!("{method} call failed"))?;
    let reply: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("{method} returned invalid JSON"))?;
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("{method}: reply had no result field"))
}

fn decode_rows<T: DeserializeOwned>(value: Value, key: &str) -> Result<Vec<T>> {
    let rows = value.get(key).cloned().unwrap_or(value);
    serde_json::from_value(rows).with_context(|| format!("decode {key}"))
}

pub fn persist_answer_blocking(request_id: String, answer: String) -> Result<String> {
    let operation = async move {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let connection_file = PathBuf::from(home).join(CONNECTION_FILE);
        let caller_directory = std::env::current_dir()?.canonicalize()?;
        let session_id = format!("gpui-spike-{}", uuid::Uuid::new_v4());
        let consumer = SubcConsumer::connect(&connection_file, ConsumerOptions::default()).await?;
        let result = call(
            &consumer,
            RouteTarget::ManagementSurface {
                module_id: "alfonso-core".into(),
            },
            BindIdentity {
                project_root: caller_directory.clone(),
                harness: "ck-app".into(),
                session: session_id.clone(),
            },
            caller_directory,
            session_id,
            "ask.persist_answer",
            json!({"requestID": request_id, "answer": answer}),
        )
        .await?;
        consumer.close().await;
        Ok(if result.get("ok").and_then(Value::as_bool) == Some(true) {
            "Answer sent".into()
        } else {
            format!(
                "Server reply: {}",
                result
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("not accepted")
            )
        })
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), operation)
            .await
            .map_err(|_| anyhow!("answer operation deadline elapsed"))?
    });
    runtime.shutdown_background();
    result
}
