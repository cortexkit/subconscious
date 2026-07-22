use std::{fs, path::PathBuf, sync::mpsc::Sender, time::Duration};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use subc_client_rs::{CallError, CallOptions, ConsumerOptions, SubcConsumer, SubscribeOptions};
use subc_protocol::{BindIdentity, RouteTarget, manifest::ProviderRole};

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

const CHAT_TURN_DEADLINE: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ChatCursor {
    pub(crate) wal_seq: u64,
    pub(crate) sub_index: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatTurnRequest {
    pub(crate) project_root: String,
    pub(crate) session_id: String,
    pub(crate) prompt: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) tools_enabled: bool,
    pub(crate) from_cursor: Option<ChatCursor>,
    pub(crate) send_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ChatErrorCause {
    pub(crate) class: String,
    pub(crate) status: Option<i64>,
    pub(crate) code: Option<String>,
    pub(crate) message: String,
    pub(crate) cause: Option<String>,
}

impl ChatErrorCause {
    pub(crate) fn render_label(&self) -> String {
        let mut parts = vec![self.class.clone()];
        if let Some(status) = self.status {
            parts.push(format!("status {status}"));
        }
        if let Some(code) = self.code.as_deref() {
            parts.push(code.to_string());
        }
        if let Some(cause) = self.cause.as_deref() {
            parts.push(format!("cause: {cause}"));
        }
        parts.join(" · ")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ChatEvent {
    RunStarted,
    ToolCall(String),
    ToolResult(String),
    TextDelta(String),
    AssistantMessage(String),
    Error(ChatErrorCause),
    RunFinished(String),
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChatTurnResult {
    pub(crate) cursor: Option<ChatCursor>,
    pub(crate) saw_event: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatTurnFailure {
    pub(crate) class: String,
    pub(crate) message: String,
    pub(crate) code: Option<String>,
    pub(crate) cause: Option<String>,
}

impl ChatTurnFailure {
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            class: "internal".into(),
            message: message.into(),
            code: None,
            cause: None,
        }
    }

    fn timeout() -> Self {
        Self {
            class: "timeout".into(),
            message: format!(
                "chat turn exceeded the {}s safety deadline",
                CHAT_TURN_DEADLINE.as_secs()
            ),
            code: Some("turn_deadline".into()),
            cause: None,
        }
    }

    fn connection(context: &str, error: impl std::fmt::Display) -> Self {
        Self {
            class: "connection".into(),
            message: format!("{context}: {error}"),
            code: None,
            cause: Some(error.to_string()),
        }
    }

    fn call(context: &str, error: CallError) -> Self {
        match error {
            CallError::Module(body) => Self {
                class: "module".into(),
                message: format!("{context}: {}", body.message),
                code: Some(body.code),
                cause: None,
            },
            CallError::OutcomeUnknown(error) => Self {
                class: "route_closed".into(),
                message: format!(
                    "{context}: route closed by the daemon mid-turn — resend to reopen"
                ),
                code: Some("outcome_unknown".into()),
                cause: Some(error.to_string()),
            },
            CallError::NotSent(error) => Self {
                class: "transport".into(),
                message: format!("{context}: request was not sent"),
                code: Some("not_sent".into()),
                cause: Some(error.to_string()),
            },
            CallError::SubscriptionBackpressure(error) => Self {
                class: "backpressure".into(),
                message: format!("{context}: the live event buffer filled"),
                code: Some("subscription_backpressure".into()),
                cause: Some(error.to_string()),
            },
            CallError::StaleRouteHandle(handle) => Self {
                class: "route_closed".into(),
                message: format!("{context}: the route handle became stale"),
                code: Some("stale_route_handle".into()),
                cause: Some(format!("{handle:?}")),
            },
        }
    }

    fn decode(error: anyhow::Error) -> Self {
        Self {
            class: "decode".into(),
            message: "broca emitted an invalid chat event".into(),
            code: Some("invalid_event".into()),
            cause: Some(format!("{error:#}")),
        }
    }

    pub(crate) fn into_error_cause(self) -> ChatErrorCause {
        ChatErrorCause {
            class: self.class,
            status: None,
            code: self.code,
            message: self.message,
            cause: self.cause,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ChatWireUpdate {
    Event(ChatEvent),
    Finished(Result<ChatTurnResult, ChatTurnFailure>),
}

pub(crate) fn run_chat_turn_blocking(
    request: ChatTurnRequest,
    updates: Sender<ChatWireUpdate>,
) -> Result<ChatTurnResult, ChatTurnFailure> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| ChatTurnFailure::connection("start chat runtime", error))?;
    let result = runtime.block_on(async {
        tokio::time::timeout(CHAT_TURN_DEADLINE, run_chat_turn(request, updates))
            .await
            .unwrap_or_else(|_| Err(ChatTurnFailure::timeout()))
    });
    runtime.shutdown_background();
    result
}

async fn run_chat_turn(
    request: ChatTurnRequest,
    updates: Sender<ChatWireUpdate>,
) -> Result<ChatTurnResult, ChatTurnFailure> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| ChatTurnFailure::internal("HOME is not set"))?;
    let connection_file = PathBuf::from(home).join(CONNECTION_FILE);
    let subscriber = SubcConsumer::connect(&connection_file, ConsumerOptions::default())
        .await
        .map_err(|error| ChatTurnFailure::connection("connect chat subscriber", error))?;
    let command = match SubcConsumer::connect(&connection_file, ConsumerOptions::default()).await {
        Ok(command) => command,
        Err(error) => {
            subscriber.close().await;
            return Err(ChatTurnFailure::connection(
                "connect chat command route",
                error,
            ));
        }
    };
    let result = run_chat_turn_connected(&subscriber, &command, request, updates).await;
    command.close().await;
    subscriber.close().await;
    result
}

async fn run_chat_turn_connected(
    subscriber: &SubcConsumer,
    command: &SubcConsumer,
    request: ChatTurnRequest,
    updates: Sender<ChatWireUpdate>,
) -> Result<ChatTurnResult, ChatTurnFailure> {
    fs::create_dir_all(&request.project_root)
        .map_err(|error| ChatTurnFailure::connection("prepare project root", error))?;
    let target = RouteTarget::ManagementSurface {
        module_id: "broca".into(),
    };
    let identity = BindIdentity {
        project_root: PathBuf::from(&request.project_root),
        harness: "runner".into(),
        session: request.session_id.clone(),
    };
    let from = request.from_cursor.map_or_else(
        || json!("start"),
        |cursor| json!({"wal_seq": cursor.wal_seq, "sub_index": cursor.sub_index}),
    );
    let subscribe_body = serde_json::to_vec(&json!({
        "method": "session.subscribe",
        "params": {"from": from},
    }))
    .map_err(|error| ChatTurnFailure::connection("encode session.subscribe", error))?;
    let subscribe_options = SubscribeOptions {
        route_open_timeout: Duration::from_secs(12),
        route_retry_deadline: Duration::from_secs(10),
        event_buffer: 512,
        ..Default::default()
    };
    let mut subscription = subscriber
        .subscribe(
            target.clone(),
            identity.clone(),
            subscribe_body,
            subscribe_options,
        )
        .await
        .map_err(|error| ChatTurnFailure::call("open session.subscribe", error))?;

    let tools = if request.tools_enabled {
        command
            .catalog_list()
            .await
            .map(tool_definitions)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let send_body = serde_json::to_vec(&json!({
        "method": "session.send",
        "params": {
            "prompt": request.prompt,
            "model": {"provider": request.provider, "model": request.model},
            "tools": tools,
            "send_id": request.send_id,
        },
    }))
    .map_err(|error| ChatTurnFailure::connection("encode session.send", error))?;
    let call_options = CallOptions {
        timeout: Duration::from_secs(12),
        route_retry_deadline: Duration::from_secs(10),
        ..Default::default()
    };
    if let Err(error) = command
        .call(target, identity, send_body, call_options)
        .await
    {
        let _ = subscription.unsubscribe();
        return Err(ChatTurnFailure::call("session.send rejected", error));
    }

    let mut cursor = request.from_cursor;
    let mut saw_event = false;
    while let Some(bytes) = subscription.events().recv().await {
        saw_event = true;
        let decoded = decode_chat_event(&bytes).map_err(ChatTurnFailure::decode)?;
        if let Some(next) = decoded.cursor {
            cursor = Some(next);
        }
        let terminal = matches!(decoded.event, ChatEvent::RunFinished(_));
        if updates.send(ChatWireUpdate::Event(decoded.event)).is_err() {
            let _ = subscription.unsubscribe();
            return Err(ChatTurnFailure::internal(
                "chat view closed during the turn",
            ));
        }
        if terminal {
            let _ = subscription.unsubscribe();
            return Ok(ChatTurnResult { cursor, saw_event });
        }
    }
    subscription
        .closed()
        .await
        .map_err(|error| ChatTurnFailure::call("session.subscribe ended", error))?;
    Ok(ChatTurnResult { cursor, saw_event })
}

fn tool_definitions(catalog: subc_client_rs::CatalogList) -> Vec<Value> {
    catalog
        .modules
        .into_iter()
        .find(|module| module.module_id == "aft")
        .into_iter()
        .flat_map(|module| module.roles)
        .filter_map(|role| match role {
            ProviderRole::ToolProvider { tools, .. } => Some(tools),
            _ => None,
        })
        .flatten()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description.unwrap_or_default(),
                "input_schema": tool.schema,
                "module": "aft",
            })
        })
        .collect()
}

struct DecodedChatEvent {
    event: ChatEvent,
    cursor: Option<ChatCursor>,
}

fn decode_chat_event(bytes: &[u8]) -> Result<DecodedChatEvent> {
    let value: Value = serde_json::from_slice(bytes).context("event is not JSON")?;
    let kind = value.get("kind").and_then(Value::as_str);
    if kind == Some("display") {
        let event = value
            .get("event")
            .and_then(Value::as_object)
            .context("display event is missing event")?;
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .context("display event is missing event.type")?;
        let event = if event_type == "text_delta" {
            ChatEvent::TextDelta(
                event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        } else {
            ChatEvent::Other
        };
        return Ok(DecodedChatEvent {
            event,
            cursor: None,
        });
    }
    if kind != Some("control") {
        return Ok(DecodedChatEvent {
            event: ChatEvent::Other,
            cursor: None,
        });
    }
    let cursor = value
        .get("cursor")
        .and_then(Value::as_object)
        .map(|cursor| -> Result<ChatCursor> {
            Ok(ChatCursor {
                wal_seq: cursor
                    .get("wal_seq")
                    .and_then(Value::as_u64)
                    .context("cursor.wal_seq must be a nonnegative integer")?,
                sub_index: cursor
                    .get("sub_index")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .context("cursor.sub_index must fit a nonnegative u32")?,
            })
        })
        .transpose()?;
    let unit = value
        .get("unit")
        .and_then(Value::as_object)
        .context("control event is missing unit")?;
    let event_type = unit
        .get("type")
        .and_then(Value::as_str)
        .context("control event is missing unit.type")?;
    let event = match event_type {
        "run_started" => ChatEvent::RunStarted,
        "tool_call" => ChatEvent::ToolCall(render_tool_call(unit)),
        "tool_result" => ChatEvent::ToolResult(
            unit.get("result")
                .and_then(|result| result.get("output"))
                .and_then(|output| output.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("tool returned no text")
                .to_string(),
        ),
        "assistant_message" => {
            let text = unit
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<String>();
            ChatEvent::AssistantMessage(text)
        }
        "error" => {
            let error = unit
                .get("error")
                .and_then(Value::as_object)
                .context("error control event is missing error")?;
            ChatEvent::Error(ChatErrorCause {
                class: error
                    .get("class")
                    .and_then(Value::as_str)
                    .unwrap_or("provider")
                    .to_string(),
                status: error.get("status").and_then(Value::as_i64),
                code: error
                    .get("code")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error")
                    .to_string(),
                cause: error
                    .get("cause")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        "run_finished" => ChatEvent::RunFinished(
            unit.get("reason")
                .and_then(Value::as_str)
                .unwrap_or("completed")
                .to_string(),
        ),
        _ => ChatEvent::Other,
    };
    Ok(DecodedChatEvent { event, cursor })
}

fn render_tool_call(unit: &serde_json::Map<String, Value>) -> String {
    let Some(call) = unit.get("call").and_then(Value::as_object) else {
        return "unknown tool()".into();
    };
    let name = call
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown tool");
    let input = call
        .get("input")
        .and_then(|input| serde_json::to_string(input).ok())
        .unwrap_or_default();
    let input = if input.chars().count() > 200 {
        format!("{}…", input.chars().take(200).collect::<String>())
    } else {
        input
    };
    format!("{name}({input})")
}

/// Read-only connectivity check for the broca and aft catalog entries used by Chat.
pub(crate) fn probe_chat_blocking() -> Result<String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(12), async {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            let connection_file = PathBuf::from(home).join(CONNECTION_FILE);
            let consumer =
                SubcConsumer::connect(&connection_file, ConsumerOptions::default()).await?;
            let catalog = consumer.catalog_list().await?;
            consumer.close().await;
            let broca = catalog
                .modules
                .iter()
                .find(|module| module.module_id == "broca")
                .context("broca is absent from the live catalog")?;
            anyhow::ensure!(
                broca
                    .roles
                    .iter()
                    .any(|role| matches!(role, ProviderRole::ManagementSurface { .. })),
                "broca does not advertise a management surface"
            );
            let tool_count = catalog
                .modules
                .iter()
                .find(|module| module.module_id == "aft")
                .map(|module| {
                    module
                        .roles
                        .iter()
                        .filter_map(|role| match role {
                            ProviderRole::ToolProvider { tools, .. } => Some(tools.len()),
                            _ => None,
                        })
                        .sum::<usize>()
                })
                .unwrap_or(0);
            Ok::<_, anyhow::Error>(format!(
                "broca management surface ready; {tool_count} aft tools"
            ))
        })
        .await
        .map_err(|_| anyhow!("chat probe deadline elapsed"))?
    });
    runtime.shutdown_background();
    result
}

#[cfg(test)]
mod chat_tests {
    use super::{ChatCursor, ChatEvent, decode_chat_event};

    #[test]
    fn chat_display_delta_decodes_without_a_cursor() {
        let decoded = decode_chat_event(
            br#"{"kind":"display","event":{"type":"text_delta","delta":"hello"}}"#,
        )
        .unwrap();
        assert_eq!(decoded.event, ChatEvent::TextDelta("hello".into()));
        assert_eq!(decoded.cursor, None);
    }

    #[test]
    fn chat_control_error_preserves_typed_cause_and_cursor() {
        let decoded = decode_chat_event(
            br#"{"kind":"control","cursor":{"wal_seq":12,"sub_index":3},"unit":{"type":"error","error":{"class":"auth","status":401,"code":"expired","message":"token expired","cause":"refresh failed"}}}"#,
        )
        .unwrap();
        assert_eq!(
            decoded.cursor,
            Some(ChatCursor {
                wal_seq: 12,
                sub_index: 3
            })
        );
        let ChatEvent::Error(error) = decoded.event else {
            panic!("expected error event");
        };
        assert_eq!(error.class, "auth");
        assert_eq!(error.status, Some(401));
        assert_eq!(error.code.as_deref(), Some("expired"));
        assert_eq!(error.cause.as_deref(), Some("refresh failed"));
    }

    #[test]
    fn chat_tool_and_terminal_controls_decode() {
        let call = decode_chat_event(
            br#"{"kind":"control","cursor":{"wal_seq":1,"sub_index":0},"unit":{"type":"tool_call","call":{"tool_name":"read","input":{"path":"a.rs"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            call.event,
            ChatEvent::ToolCall("read({\"path\":\"a.rs\"})".into())
        );
        let finished = decode_chat_event(
            br#"{"kind":"control","cursor":{"wal_seq":2,"sub_index":0},"unit":{"type":"run_finished","reason":"interrupted"}}"#,
        )
        .unwrap();
        assert_eq!(finished.event, ChatEvent::RunFinished("interrupted".into()));
    }
}
