#![forbid(unsafe_code)]

//! Real-daemon test harness, not a privileged-write template. This deliberately
//! copies caller-supplied text into the summary; real modules must construct the
//! summary themselves and authorise only the unchanged in-flight write.

use serde_json::{json, Value};
use std::{
    error::Error,
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use subc_client_rs::{
    async_trait, CallOptions, ConsumerOptions, HandlerOutcome, ModuleHandler, OperatorConfirmError,
    RequestCtx, RouteHandle, SubcConsumer,
};
use subc_protocol::{
    manifest::{Concurrency, ExecutionMode, IdentityScope, ModuleManifest, ProviderRole, Tool},
    BindIdentity, ModuleHelloAckBody, Principal, RouteTarget,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // Read the descriptor before starting any task or consumer connection.
    let _ = subc_client_rs::launch_nonce()?;
    let module_id = std::env::var(subc_protocol::SUBC_MODULE_ID_ENV)?;
    let connection_file = std::env::args_os()
        .skip_while(|arg| arg != "--subc")
        .nth(1)
        .map(PathBuf::from)
        .ok_or("missing --subc <connection-file>")?;
    let shared = Arc::new(Mutex::new(None));
    let (handle, serving) = subc_client_rs::serve_with_handle(
        &connection_file,
        manifest(&module_id),
        Harness {
            previous: Mutex::new(None),
            connection_file: connection_file.clone(),
            handle: shared.clone(),
            events: std::env::var_os("OPERATOR_MODULE_EVENTS").map(PathBuf::from),
        },
    )
    .await?;
    *shared.lock().unwrap() = Some(handle);
    serving.await?;
    Ok(())
}

struct Harness {
    handle: Arc<Mutex<Option<subc_client_rs::ModuleHandle>>>,
    previous: Mutex<Option<RouteHandle>>,
    connection_file: PathBuf,
    events: Option<PathBuf>,
}

impl Harness {
    fn record(&self, value: Value) {
        if let Some(path) = &self.events {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            file.write_all(format!("{value}\n").as_bytes()).unwrap();
        }
    }
}

#[async_trait]
impl ModuleHandler for Harness {
    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        self.record(json!({"event":"hello_ack", "subc_ops":ack.subc_ops}));
    }

    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let handle = self.handle.lock().unwrap().as_ref().unwrap().clone();
        let request: Value = serde_json::from_slice(&body).unwrap();
        let mode = request["mode"].as_str().unwrap_or("confirm");
        let summary = request["summary"].as_str().unwrap_or("test write");
        let route = {
            let mut previous = self.previous.lock().unwrap();
            let route = if mode == "stale_route" {
                previous
                    .clone()
                    .unwrap_or_else(|| ctx.route_handle().clone())
            } else {
                ctx.route_handle().clone()
            };
            *previous = Some(ctx.route_handle().clone());
            route
        };
        if mode == "echo" {
            return HandlerOutcome::Response(body);
        }
        if mode == "describe" {
            let reply = handle
                .scope_describe(
                    Principal::Reserved {
                        module_id: "operator-confirm-module-1".into(),
                    },
                    "unknown-scope".into(),
                )
                .await;
            return HandlerOutcome::Response(
                serde_json::to_vec(&json!({"ok":reply.is_ok()})).unwrap(),
            );
        }
        if mode == "forward" {
            let consumer = SubcConsumer::connect(&self.connection_file, ConsumerOptions::default())
                .await
                .unwrap();
            let options = CallOptions {
                timeout: Duration::from_secs(300),
                ..CallOptions::default()
            };
            let target = RouteTarget::ToolProvider {
                module_id: request["peer"].as_str().unwrap().into(),
            };
            let reply = consumer.call(target, BindIdentity::new(std::env::temp_dir(), "operator-harness", "forward"), serde_json::to_vec(&json!({"summary":summary, "mode":request["peer_mode"].as_str().unwrap_or("confirm")})).unwrap(), options).await;
            consumer.close().await;
            return match reply {
                Ok(reply) => HandlerOutcome::Response(reply),
                Err(error) => HandlerOutcome::Error {
                    code: "forward_failed".into(),
                    message: error.to_string(),
                },
            };
        }
        let confirm = handle.confirm_operator(summary, &route);
        tokio::pin!(confirm);
        // Expose a wire-admission barrier for FIFO tests: on the first poll the
        // helper queues its control frame before yielding for the daemon reply.
        let ready = std::future::poll_fn(|cx| {
            use std::{future::Future, task::Poll};
            Poll::Ready(match confirm.as_mut().poll(cx) {
                Poll::Ready(result) => Some(result),
                Poll::Pending => None,
            })
        })
        .await;
        if let Some(result) = ready {
            return answer(result);
        }
        self.record(json!({"event":"confirm_waiting", "summary":summary}));
        if mode == "exit_when_shown" {
            let path = PathBuf::from(request["events"].as_str().unwrap());
            tokio::select! {
                result = &mut confirm => return answer(result),
                _ = async {
                    loop {
                        if fs::read_to_string(&path).unwrap_or_default().lines().any(|line| {
                            serde_json::from_str::<Value>(line).is_ok_and(|e| e["event"] == "prompt_shown")
                        }) { std::process::exit(0); }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } => unreachable!(),
            }
        }
        tokio::select! {
            result = &mut confirm => answer(result),
            _ = ctx.cancelled() => HandlerOutcome::Error {code:"cancelled".into(), message:"caller left".into()},
        }
    }
}

fn answer(result: Result<(), OperatorConfirmError>) -> HandlerOutcome {
    let name = match result {
        Ok(()) => "confirmed",
        Err(OperatorConfirmError::Declined) => "declined",
        Err(OperatorConfirmError::PresenceUnavailable) => "unavailable",
        Err(OperatorConfirmError::SummaryInvalid) => "summary_invalid",
        Err(OperatorConfirmError::NotPermitted) => "not_permitted",
        Err(OperatorConfirmError::Unsupported) => "unsupported",
        Err(_) => "unexpected",
    };
    HandlerOutcome::Response(serde_json::to_vec(&json!({"result":name})).unwrap())
}

fn manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest::builder(module_id, env!("CARGO_PKG_VERSION"))
        .provides(vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "operator-test".into(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: json!({"type":"object"}),
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: false,
            sub_supervises: false,
        }])
        .build()
}
