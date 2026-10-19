use super::*;
use serde_json::json;

#[derive(Clone, Default)]
struct BindRecorder(Arc<Mutex<Option<RouteBindRequest>>>);

#[async_trait]
impl ModuleHandler for BindRecorder {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }

    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        *self.0.lock().unwrap() = Some(request.clone());
        BindDecision::accept()
    }
}

fn scope(attributes: serde_json::Value) -> serde_json::Value {
    json!({
        "owner": { "kind": "reserved", "module_id": "scope-owner" },
        "ref": "scope-a",
        "scope_epoch": 1,
        "kind": "ephemeral",
        "attributes": attributes,
        "owner_authorized": true,
    })
}

async fn decoded_bind(scope: Option<serde_json::Value>, mode: Option<&str>) -> RouteBindRequest {
    let recorder = BindRecorder::default();
    let handler = recorder.clone();
    let mut served = stand_in::serve_against_stand_in_with_ops(|_| handler, &[]).await;
    let mut body = json!({
        "op": "route.bind",
        "route_channel": 7,
        "epoch": 1,
        "target": RouteTarget::ToolProvider { module_id: "provider".to_string() },
        "identity": BindIdentity::new("/tmp/project", "test", "mode"),
    });
    if let Some(scope) = scope {
        body["scope"] = scope;
    }
    if let Some(mode) = mode {
        body["call_mode"] = json!(mode);
    }
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        2,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(&mut served.daemon, &frame).await.unwrap();
    served.daemon.flush().await.unwrap();
    let ack = timeout(Duration::from_secs(2), read_frame(&mut served.daemon))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        ack.header.ty,
        FrameType::Response,
        "{}",
        String::from_utf8_lossy(&ack.body)
    );
    assert_eq!(ack.header.corr, 2);
    let request = recorder.0.lock().unwrap().clone().expect("on_bind ran");
    served.handle.close_connection();
    assert!(timeout(Duration::from_secs(2), served.serve)
        .await
        .unwrap()
        .unwrap()
        .is_ok());
    request
}

#[tokio::test]
async fn on_behalf_of_agent_is_exposed_to_bind_handlers_without_agent_authority() {
    let request = decoded_bind(
        Some(scope(json!({"on_behalf_of_agent": "head-agent"}))),
        None,
    )
    .await;
    assert_eq!(request.call_mode, CallMode::Classic);
    let attributes = request.scope.unwrap().attributes;
    assert_eq!(attributes.on_behalf_of_agent.as_deref(), Some("head-agent"));
    assert_eq!(attributes.agent_id, None);
    assert!(!attributes.delegates);
}

#[tokio::test]
async fn absent_call_mode_on_a_run_bind_reads_codemode() {
    let request = decoded_bind(Some(scope(json!({ "run_id": "run-a" }))), None).await;
    assert_eq!(request.call_mode, CallMode::Codemode);
    assert_eq!(
        request.scope.unwrap().attributes.run_id.as_deref(),
        Some("run-a")
    );
}

#[tokio::test]
async fn absent_call_mode_on_a_flow_bind_reads_flow() {
    let request = decoded_bind(Some(scope(json!({ "flow_id": "flow-a" }))), None).await;
    assert_eq!(request.call_mode, CallMode::Flow);
}

#[tokio::test]
async fn absent_call_mode_on_plain_and_unscoped_binds_reads_classic() {
    assert_eq!(decoded_bind(None, None).await.call_mode, CallMode::Classic);
    assert_eq!(
        decoded_bind(Some(scope(json!({}))), None).await.call_mode,
        CallMode::Classic
    );
}

#[tokio::test]
async fn explicit_call_modes_are_preserved_including_unknown() {
    for (wire, expected) in [
        ("classic", CallMode::Classic),
        ("codemode", CallMode::Codemode),
        ("flow", CallMode::Flow),
        ("future-mode", CallMode::Unknown("future-mode".to_string())),
    ] {
        let request = decoded_bind(Some(scope(json!({ "run_id": "run-a" }))), Some(wire)).await;
        assert_eq!(request.call_mode, expected);
    }
}

#[test]
fn bind_builders_derive_scope_modes_and_allow_explicit_override() {
    let bind = || {
        RouteBindRequest::new(
            RouteHandle::detached(7, 1),
            RouteTarget::ToolProvider {
                module_id: "provider".to_string(),
            },
            BindIdentity::new("/tmp/project", "test", "mode"),
        )
    };
    assert_eq!(bind().call_mode, CallMode::Classic);
    for (attributes, expected) in [
        (json!({}), CallMode::Classic),
        (json!({ "run_id": "run-a" }), CallMode::Codemode),
        (json!({ "flow_id": "flow-a" }), CallMode::Flow),
    ] {
        let stamp = serde_json::from_value(scope(attributes)).unwrap();
        let scoped = bind().with_scope(stamp);
        assert_eq!(scoped.call_mode, expected);
        assert_eq!(
            scoped.with_call_mode(CallMode::Classic).call_mode,
            CallMode::Classic
        );
    }
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn module_harness_preserves_explicit_call_mode() {
    let recorder = BindRecorder::default();
    let handler = recorder.clone();
    let mut harness = test_support::ModuleHarness::start(|_| handler, []).await;
    let request = RouteBindRequest::new(
        RouteHandle::detached(7, 1),
        RouteTarget::ToolProvider {
            module_id: "provider".to_string(),
        },
        BindIdentity::new("/tmp/project", "test", "mode"),
    )
    .with_scope(serde_json::from_value(scope(json!({ "run_id": "run-a" }))).unwrap())
    .with_call_mode(CallMode::Flow);
    harness.bind_route(request).await;
    assert_eq!(
        recorder.0.lock().unwrap().as_ref().unwrap().call_mode,
        CallMode::Flow
    );
    harness.shutdown().await;
}
