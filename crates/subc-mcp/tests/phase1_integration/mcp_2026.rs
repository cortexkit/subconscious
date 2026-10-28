//! MCP 2026-07-28 hosts: no `initialize`, and module questions travel as MRTR
//! `inputRequests` on the tools/call they belong to instead of as
//! server-to-client requests (docs/reverse-requests.md §5.1.1).

use super::*;
use rmcp::model::{
    CallToolResponse, InputRequiredResult, InputResponses, ServerNotification, SubscriptionFilter,
};

async fn start_2026(label: &str, module_env: &[(&str, &str)]) -> ReverseHarness {
    let mut harness =
        ReverseHarness::start_inner(label, elicitation_capabilities(), module_env, false).await;
    let client = harness
        .shim
        .serve_mcp_client_2026(harness.client_handler.clone())
        .await;
    harness.client = Some(client);
    harness
}

/// A form-mode `elicitation/create` reverse request, optionally naming the
/// forward request it belongs to.
fn form_elicitation_body(label: &str, in_reply_to: Option<u64>) -> Value {
    let mut body = json!({
        "method": "elicitation/create",
        "params": {
            "message": format!("approve {label}"),
            "requestedSchema": {
                "type": "object",
                "properties": { "approved": { "type": "boolean" } }
            }
        }
    });
    if let Some(corr) = in_reply_to {
        body["in_reply_to"] = json!(corr);
    }
    body
}

fn text_result(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn result_text(result: &impl serde::Serialize) -> Value {
    serde_json::to_value(result)
        .unwrap()
        .pointer("/content/0/text")
        .cloned()
        .unwrap_or(Value::Null)
}

fn spawn_call_once(
    harness: &ReverseHarness,
    params: CallToolRequestParams,
) -> JoinHandle<Result<CallToolResponse, ServiceError>> {
    let peer = harness.client.as_ref().unwrap().peer().clone();
    tokio::spawn(async move { peer.call_tool_once(params).await })
}

async fn expect_input_required(
    call: JoinHandle<Result<CallToolResponse, ServiceError>>,
) -> InputRequiredResult {
    match timeout(READ_TIMEOUT, call).await {
        Ok(Ok(Ok(CallToolResponse::InputRequired(result)))) => result,
        other => panic!("expected an input_required answer, got {other:?}"),
    }
}

async fn expect_complete_text(call: JoinHandle<Result<CallToolResponse, ServiceError>>) -> Value {
    match timeout(READ_TIMEOUT, call).await {
        Ok(Ok(Ok(CallToolResponse::Complete(result)))) => result_text(&result),
        other => panic!("expected a complete tool result, got {other:?}"),
    }
}

/// The follow-up call a host makes after asking the person: the same tool,
/// the answer under the question's key, and the server's `requestState`.
fn follow_up(name: &str, asked: &InputRequiredResult, answer: Value) -> CallToolRequestParams {
    let key = asked
        .input_requests
        .as_ref()
        .and_then(|requests| requests.keys().next().cloned())
        .expect("input_required answer should carry one input request");
    let mut responses = InputResponses::new();
    responses.insert(key, answer);
    let mut params = CallToolRequestParams::new(name.to_owned());
    params.input_responses = Some(responses);
    params.request_state = asked.request_state.clone();
    params
}

#[tokio::test]
async fn mcp_2026_host_lists_calls_and_answers_elicitation_through_mrtr() {
    let mut harness = start_2026("mcp-2026-mrtr-roundtrip", &[]).await;
    let tool_name = harness.provider.exposed_tool_name();
    let client = harness.client.take().unwrap();

    let tools = client.peer().list_tools(None).await.unwrap();
    assert!(
        tools.tools.iter().any(|tool| tool.name == tool_name),
        "2026-07-28 tools/list should list the provider's tool: {tools:?}"
    );
    let route = harness.provider.wait_bound().await;

    // rmcp's `call_tool` fulfils `inputRequests` through the client handler
    // and repeats the call, as a 2026-07-28 host does.
    let call = client.call_tool(CallToolRequestParams::new(tool_name.clone()));
    let provider = &mut harness.provider;
    let script = async {
        let forward = provider.wait_route_request().await;
        provider
            .send_reverse_request(route, 911, form_elicitation_body("roundtrip", None))
            .await;
        let answer = provider.wait_reverse_response(911).await;
        provider
            .send_route_response(&forward, text_result("approved-ok"))
            .await;
        answer
    };
    let handler = harness.client_handler.clone();
    let person = async {
        handler.wait_for_prompts(1).await;
        handler
            .respond_elicitation(accepted_elicitation(json!({ "approved": true })))
            .await;
    };
    let (result, answer, ()) = timeout(READ_TIMEOUT, async { tokio::join!(call, script, person) })
        .await
        .expect("MRTR round trip should finish");

    assert_eq!(answer.get("action"), Some(&json!("accept")));
    assert_eq!(answer.pointer("/content/approved"), Some(&json!(true)));
    let result = result.expect("the repeated tools/call should complete");
    assert_eq!(result_text(&result), json!("approved-ok"));
    harness.client_handler.assert_prompt_count_stays(1).await;

    harness.client = Some(client);
    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_2026_linked_question_rides_the_named_call_while_another_is_in_flight() {
    let mut harness = start_2026("mcp-2026-mrtr-linked", &[]).await;
    let tool_name = harness.provider.exposed_tool_name();
    let route = harness.provider.wait_bound().await;

    let first = spawn_call_once(&harness, CallToolRequestParams::new(tool_name.clone()));
    let first_forward = harness.provider.wait_route_request().await;
    let second = spawn_call_once(&harness, CallToolRequestParams::new(tool_name.clone()));
    let second_forward = harness.provider.wait_route_request().await;

    harness
        .provider
        .send_reverse_request(
            route,
            921,
            form_elicitation_body("linked", Some(second_forward.header.corr)),
        )
        .await;
    let asked = expect_input_required(second).await;
    assert!(
        !first.is_finished(),
        "the question must not be answered on the other call"
    );

    harness
        .provider
        .send_route_response(&first_forward, text_result("first"))
        .await;
    assert_eq!(expect_complete_text(first).await, json!("first"));

    let retry = spawn_call_once(
        &harness,
        follow_up(
            &tool_name,
            &asked,
            json!({ "action": "accept", "content": { "approved": true } }),
        ),
    );
    let answer = harness.provider.wait_reverse_response(921).await;
    assert_eq!(answer.pointer("/content/approved"), Some(&json!(true)));
    harness
        .provider
        .send_route_response(&second_forward, text_result("second"))
        .await;
    assert_eq!(expect_complete_text(retry).await, json!("second"));

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_2026_unlinked_question_with_two_calls_waiting_fails_closed() {
    let mut harness = start_2026("mcp-2026-mrtr-ambiguous", &[]).await;
    let tool_name = harness.provider.exposed_tool_name();
    let route = harness.provider.wait_bound().await;

    let first = spawn_call_once(&harness, CallToolRequestParams::new(tool_name.clone()));
    let first_forward = harness.provider.wait_route_request().await;
    let second = spawn_call_once(&harness, CallToolRequestParams::new(tool_name.clone()));
    let second_forward = harness.provider.wait_route_request().await;

    harness
        .provider
        .send_reverse_request(route, 931, form_elicitation_body("ambiguous", None))
        .await;
    let error = harness.provider.wait_reverse_error(931).await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("2 tool calls are waiting")),
        "unexpected refusal: {error}"
    );

    // Neither call was turned into a question; both finish normally.
    harness
        .provider
        .send_route_response(&first_forward, text_result("first"))
        .await;
    harness
        .provider
        .send_route_response(&second_forward, text_result("second"))
        .await;
    assert_eq!(expect_complete_text(first).await, json!("first"));
    assert_eq!(expect_complete_text(second).await, json!("second"));
    harness.client_handler.assert_prompt_count_stays(0).await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_2026_unanswered_question_is_declined_after_the_relay_ttl() {
    let mut harness = start_2026(
        "mcp-2026-mrtr-ttl",
        &[("SUBC_MCP_REVERSE_RELAY_TTL_MS", "300")],
    )
    .await;
    let tool_name = harness.provider.exposed_tool_name();
    let route = harness.provider.wait_bound().await;

    let call = spawn_call_once(&harness, CallToolRequestParams::new(tool_name.clone()));
    harness.provider.wait_route_request().await;
    harness
        .provider
        .send_reverse_request(route, 951, form_elicitation_body("ttl", None))
        .await;
    let asked = expect_input_required(call).await;

    // The host never comes back: the module hears a decline.
    let answer = harness.provider.wait_reverse_response(951).await;
    assert_eq!(answer, json!({ "action": "decline" }));

    // The expired state resumes nothing.
    let late = spawn_call_once(
        &harness,
        follow_up(&tool_name, &asked, json!({ "action": "accept" })),
    );
    match timeout(READ_TIMEOUT, late).await {
        Ok(Ok(Err(ServiceError::McpError(error)))) => {
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS, "{error:?}")
        }
        other => panic!("expected the expired requestState to be refused, got {other:?}"),
    }

    harness.shutdown().await;
}

/// Reads gateway messages until the response to `id`, keeping every message
/// seen on the way.
async fn read_raw_until_response(
    reader: &mut ChildStdout,
    id: u64,
    seen: &mut Vec<Value>,
) -> Value {
    loop {
        let message = read_raw_mcp_message(reader).await;
        seen.push(message.clone());
        if message.get("method").is_none() && message.get("id") == Some(&json!(id)) {
            return message;
        }
    }
}

fn raw_2026_request(id: u64, method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "subc-mcp-raw-2026-test", "version": "0" },
        "io.modelcontextprotocol/clientCapabilities": {
            "elicitation": {}, "sampling": {}, "roots": {}
        }
    });
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

#[tokio::test]
async fn mcp_2026_host_is_never_sent_a_server_to_client_request() {
    // A raw host, so every byte the gateway writes is inspected. It declares
    // elicitation, sampling and roots, which an `initialize` host would be
    // asked for directly.
    let mut harness = ReverseHarness::start_attach_window("mcp-2026-no-server-requests").await;
    let tool_name = harness.provider.exposed_tool_name();
    let mut stdin = harness.shim.stdin.take().unwrap();
    let mut stdout = harness.shim.stdout.take().unwrap();
    let mut seen = Vec::new();

    write_raw_mcp_message(&mut stdin, &raw_2026_request(1, "tools/list", json!({}))).await;
    let listed = read_raw_until_response(&mut stdout, 1, &mut seen).await;
    assert!(
        listed.pointer("/result/tools").is_some(),
        "tools/list failed: {listed}"
    );
    let route = harness.provider.wait_bound().await;

    write_raw_mcp_message(
        &mut stdin,
        &raw_2026_request(
            2,
            "tools/call",
            json!({ "name": tool_name, "arguments": {} }),
        ),
    )
    .await;
    let forward = harness.provider.wait_route_request().await;

    // Sampling and roots have no MRTR path in the gateway: refused at once.
    harness
        .provider
        .send_reverse_request(
            route,
            941,
            json!({ "method": "sampling/createMessage", "params": { "messages": [], "maxTokens": 1 } }),
        )
        .await;
    let error = harness.provider.wait_reverse_error(941).await;
    assert_eq!(error.get("code"), Some(&json!(-32601)));
    harness
        .provider
        .send_reverse_request(route, 942, json!({ "method": "roots/list", "params": {} }))
        .await;
    let error = harness.provider.wait_reverse_error(942).await;
    assert_eq!(error.get("code"), Some(&json!(-32601)));

    // Elicitation comes back as the tools/call answer.
    harness
        .provider
        .send_reverse_request(route, 943, form_elicitation_body("raw", None))
        .await;
    let asked = read_raw_until_response(&mut stdout, 2, &mut seen).await;
    assert_eq!(
        asked.pointer("/result/resultType"),
        Some(&json!("input_required")),
        "{asked}"
    );
    let (key, question) = asked
        .pointer("/result/inputRequests")
        .and_then(Value::as_object)
        .and_then(|requests| requests.iter().next())
        .map(|(key, question)| (key.clone(), question.clone()))
        .expect("one input request");
    assert_eq!(question.get("method"), Some(&json!("elicitation/create")));
    let state = asked
        .pointer("/result/requestState")
        .cloned()
        .expect("requestState");

    write_raw_mcp_message(
        &mut stdin,
        &raw_2026_request(
            3,
            "tools/call",
            json!({
                "name": tool_name,
                "arguments": {},
                "inputResponses": { key: { "action": "accept", "content": { "approved": true } } },
                "requestState": state,
            }),
        ),
    )
    .await;
    let answer = harness.provider.wait_reverse_response(943).await;
    assert_eq!(answer.pointer("/content/approved"), Some(&json!(true)));
    harness
        .provider
        .send_route_response(&forward, text_result("raw-ok"))
        .await;
    let done = read_raw_until_response(&mut stdout, 3, &mut seen).await;
    assert_eq!(
        done.pointer("/result/content/0/text"),
        Some(&json!("raw-ok"))
    );

    // Nothing the gateway wrote was a request (a message with both a method
    // and an id), and nothing was a notification the host did not ask for.
    for message in &seen {
        assert!(
            message.get("method").is_none(),
            "gateway sent a 2026-07-28 host an unsolicited message: {message}"
        );
    }

    harness.shim.stdin = Some(stdin);
    harness.shim.stdout = Some(stdout);
    drop(harness.shim.stdin.take());
    harness.shutdown().await;
}

/// Registers a second provider module, which changes the session's tool set,
/// and waits until the 2026-07-28 host's tools/list shows its tool.
async fn add_provider_and_wait_listed(harness: &ReverseHarness) -> ScriptedProvider {
    let late = ScriptedProvider::start(
        &harness._server.daemon.connection_file_path,
        "late",
        "probe",
    )
    .await;
    wait_for_registration(&harness._server, "late").await;
    let peer = harness.client.as_ref().unwrap().peer().clone();
    let late_tool = late.exposed_tool_name();
    timeout(READ_TIMEOUT, async {
        loop {
            let tools = peer.list_tools(None).await.expect("tools/list");
            if tools.tools.iter().any(|tool| tool.name == late_tool) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the new provider's tool should be listed");
    late
}

#[tokio::test]
async fn mcp_2026_tools_subscription_receives_tools_list_changed() {
    let mut harness = start_2026("mcp-2026-listen-tools", &[]).await;
    harness.provider.wait_bound().await;
    let peer = harness.client.as_ref().unwrap().peer().clone();
    let mut subscription = peer
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await
        .expect("subscriptions/listen should be acknowledged");
    assert_eq!(subscription.acknowledged().tools_list_changed, Some(true));

    let late = add_provider_and_wait_listed(&harness).await;
    let notification = timeout(READ_TIMEOUT, subscription.next())
        .await
        .expect("a tools/list_changed should arrive on the stream")
        .expect("the stream should stay healthy")
        .expect("the stream should stay open");
    assert!(
        matches!(
            notification,
            ServerNotification::ToolListChangedNotification(_)
        ),
        "unexpected notification: {notification:?}"
    );

    drop(subscription);
    late.shutdown().await;
    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_2026_prompts_subscription_does_not_receive_tools_list_changed() {
    let mut harness = start_2026("mcp-2026-listen-prompts", &[]).await;
    harness.provider.wait_bound().await;
    let peer = harness.client.as_ref().unwrap().peer().clone();
    let mut subscription = peer
        .listen(SubscriptionFilter::builder().prompts_list_changed().build())
        .await
        .expect("subscriptions/listen should be acknowledged");
    assert_eq!(subscription.acknowledged().prompts_list_changed, Some(true));
    assert_eq!(subscription.acknowledged().tools_list_changed, None);

    // The tool set does change (the host lists the new tool), but this stream
    // asked only for prompt changes.
    let late = add_provider_and_wait_listed(&harness).await;
    match timeout(QUIET_TIMEOUT, subscription.next()).await {
        Err(_) => {}
        Ok(other) => panic!("a prompts-only stream received {other:?}"),
    }

    drop(subscription);
    late.shutdown().await;
    harness.shutdown().await;
}

// MCP tasks (the io.modelcontextprotocol/tasks extension): a call that
// outlives the task promotion delay, or whose module asks a question, is
// answered with a task handle the host polls.

use rmcp::model::{
    CancelTaskParams, CreateTaskResult, GetTaskParams, GetTaskResult, TaskPayload, TaskStatus,
    UpdateTaskParams, TASKS_EXTENSION_ID,
};

/// A 2026-07-28 host that declares elicitation and the tasks extension on
/// every request.
async fn start_2026_tasks(label: &str, module_env: &[(&str, &str)]) -> ReverseHarness {
    let mut capabilities = elicitation_capabilities();
    capabilities.extensions = Some(
        serde_json::from_value(json!({ TASKS_EXTENSION_ID: {} }))
            .expect("the tasks extension capability should parse"),
    );
    let mut harness = ReverseHarness::start_inner(label, capabilities, module_env, false).await;
    let client = harness
        .shim
        .serve_mcp_client_2026(harness.client_handler.clone())
        .await;
    harness.client = Some(client);
    harness
}

async fn expect_task(call: JoinHandle<Result<CallToolResponse, ServiceError>>) -> CreateTaskResult {
    match timeout(READ_TIMEOUT, call).await {
        Ok(Ok(Ok(CallToolResponse::Task(task)))) => task,
        other => panic!("expected a task handle, got {other:?}"),
    }
}

async fn get_task(harness: &ReverseHarness, task_id: &str) -> Result<GetTaskResult, ServiceError> {
    let peer = harness.client.as_ref().unwrap().peer().clone();
    peer.get_task(GetTaskParams::new(task_id)).await
}

/// Polls `tasks/get` until the task reaches `status`, failing if it ends in
/// another terminal status first.
async fn poll_task_until(
    harness: &ReverseHarness,
    task_id: &str,
    status: TaskStatus,
) -> GetTaskResult {
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        let task = get_task(harness, task_id).await.expect("tasks/get");
        let current = task.task.status();
        if current == status {
            return task;
        }
        assert!(
            !current.is_terminal(),
            "task {task_id} ended as {current:?}, expected {status:?}: {task:?}"
        );
        assert!(
            Instant::now() < deadline,
            "task {task_id} never reached {status:?}; last seen {current:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// Fails if the module receives another tool call within the quiet window:
/// a task must never send its call again.
async fn assert_no_further_route_request(provider: &mut ScriptedProvider) {
    let deadline = Instant::now() + QUIET_TIMEOUT;
    while let Ok(Some(event)) = timeout(
        deadline.saturating_duration_since(Instant::now()),
        provider.events_rx.recv(),
    )
    .await
    {
        assert!(
            !matches!(event, ScriptedProviderEvent::RouteRequest(_)),
            "the module received the call again: {event:?}"
        );
    }
}

#[tokio::test]
async fn mcp_2026_long_tool_call_runs_as_a_task_polled_to_its_result() {
    let mut harness = start_2026_tasks(
        "mcp-2026-task-poll",
        &[("SUBC_MCP_TASK_PROMOTION_DELAY_MS", "1000")],
    )
    .await;
    let tool_name = harness.provider.exposed_tool_name();
    harness.provider.wait_bound().await;

    // A call the module answers within the delay is answered inline.
    let quick = spawn_call_once(&harness, CallToolRequestParams::new(tool_name.clone()));
    let forward = harness.provider.wait_route_request().await;
    harness
        .provider
        .send_route_response(&forward, text_result("quick"))
        .await;
    assert_eq!(expect_complete_text(quick).await, json!("quick"));

    // A call still running after the delay becomes a task.
    let call = spawn_call_once(&harness, CallToolRequestParams::new(tool_name.clone()));
    let forward = harness.provider.wait_route_request().await;
    let created = expect_task(call).await;
    assert_eq!(created.task.status, TaskStatus::Working);
    assert_eq!(created.task.poll_interval_ms, Some(1000));
    assert_eq!(
        created.task.ttl_ms, None,
        "a running task is kept for as long as its call runs"
    );
    let task_id = created.task.task_id.clone();
    let working = get_task(&harness, &task_id).await.expect("tasks/get");
    assert_eq!(working.task.status(), TaskStatus::Working);

    harness
        .provider
        .send_route_response(&forward, text_result("long-ok"))
        .await;
    let done = poll_task_until(&harness, &task_id, TaskStatus::Completed).await;
    match &done.task.payload {
        TaskPayload::Completed { result } => {
            assert_eq!(result_text(result), json!("long-ok"));
        }
        other => panic!("expected the module's result, got {other:?}"),
    }
    assert!(
        done.task.task.ttl_ms.is_some(),
        "a finished task reports when it will be dropped"
    );
    assert_no_further_route_request(&mut harness.provider).await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_2026_cancelling_a_task_cancels_the_module_call() {
    let mut harness = start_2026_tasks(
        "mcp-2026-task-cancel",
        &[("SUBC_MCP_TASK_PROMOTION_DELAY_MS", "200")],
    )
    .await;
    let tool_name = harness.provider.exposed_tool_name();
    harness.provider.wait_bound().await;

    let call = spawn_call_once(&harness, CallToolRequestParams::new(tool_name));
    let forward = harness.provider.wait_route_request().await;
    let task_id = expect_task(call).await.task.task_id;

    let peer = harness.client.as_ref().unwrap().peer().clone();
    peer.cancel_task(CancelTaskParams::new(task_id.clone()))
        .await
        .expect("tasks/cancel should be acknowledged");
    harness
        .provider
        .wait_route_cancel(forward.header.corr)
        .await;
    poll_task_until(&harness, &task_id, TaskStatus::Cancelled).await;

    // A result the module sends anyway changes nothing.
    harness
        .provider
        .send_route_response(&forward, text_result("too-late"))
        .await;
    sleep(QUIET_TIMEOUT).await;
    let after = get_task(&harness, &task_id).await.expect("tasks/get");
    assert_eq!(after.task.status(), TaskStatus::Cancelled);
    assert_no_further_route_request(&mut harness.provider).await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_2026_module_question_goes_through_input_required_and_back() {
    // A delay no test waits out: the question alone makes the call a task.
    let mut harness = start_2026_tasks(
        "mcp-2026-task-input",
        &[("SUBC_MCP_TASK_PROMOTION_DELAY_MS", "600000")],
    )
    .await;
    let tool_name = harness.provider.exposed_tool_name();
    let route = harness.provider.wait_bound().await;

    let call = spawn_call_once(&harness, CallToolRequestParams::new(tool_name));
    let forward = harness.provider.wait_route_request().await;
    harness
        .provider
        .send_reverse_request(
            route,
            961,
            form_elicitation_body("task", Some(forward.header.corr)),
        )
        .await;
    let task_id = expect_task(call).await.task.task_id;

    let asked = poll_task_until(&harness, &task_id, TaskStatus::InputRequired).await;
    let key = match &asked.task.payload {
        TaskPayload::InputRequired { input_requests } => {
            assert_eq!(input_requests.len(), 1, "{input_requests:?}");
            input_requests.keys().next().unwrap().clone()
        }
        other => panic!("expected the module's question, got {other:?}"),
    };

    let mut responses = InputResponses::new();
    responses.insert(
        key,
        json!({ "action": "accept", "content": { "approved": true } }),
    );
    let peer = harness.client.as_ref().unwrap().peer().clone();
    peer.update_task(UpdateTaskParams::new(task_id.clone(), responses))
        .await
        .expect("tasks/update should be acknowledged");
    let answer = harness.provider.wait_reverse_response(961).await;
    assert_eq!(answer.pointer("/content/approved"), Some(&json!(true)));
    poll_task_until(&harness, &task_id, TaskStatus::Working).await;

    harness
        .provider
        .send_route_response(&forward, text_result("answered-ok"))
        .await;
    let done = poll_task_until(&harness, &task_id, TaskStatus::Completed).await;
    match &done.task.payload {
        TaskPayload::Completed { result } => {
            assert_eq!(result_text(result), json!("answered-ok"))
        }
        other => panic!("expected the module's result, got {other:?}"),
    }
    // The host was asked through the task, never through its handler.
    harness.client_handler.assert_prompt_count_stays(0).await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_2026_finished_task_is_reported_expired_after_its_ttl() {
    let mut harness = start_2026_tasks(
        "mcp-2026-task-expiry",
        &[
            ("SUBC_MCP_TASK_PROMOTION_DELAY_MS", "200"),
            ("SUBC_MCP_TASK_RESULT_TTL_MS", "400"),
        ],
    )
    .await;
    let tool_name = harness.provider.exposed_tool_name();
    harness.provider.wait_bound().await;

    let call = spawn_call_once(&harness, CallToolRequestParams::new(tool_name));
    let forward = harness.provider.wait_route_request().await;
    let task_id = expect_task(call).await.task.task_id;
    harness
        .provider
        .send_route_response(&forward, text_result("kept-briefly"))
        .await;
    poll_task_until(&harness, &task_id, TaskStatus::Completed).await;

    sleep(Duration::from_millis(800)).await;
    match get_task(&harness, &task_id).await {
        Err(ServiceError::McpError(error)) => {
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS, "{error:?}");
            assert!(error.message.contains("expired"), "{error:?}");
        }
        other => panic!("expected the task to be reported expired, got {other:?}"),
    }
    // An id the session never handed out is unknown, not expired.
    match get_task(&harness, "subc-mcp-task-9999").await {
        Err(ServiceError::McpError(error)) => {
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS, "{error:?}");
            assert!(error.message.contains("unknown task"), "{error:?}");
        }
        other => panic!("expected an unknown task, got {other:?}"),
    }

    harness.shutdown().await;
}
