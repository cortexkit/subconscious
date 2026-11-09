use super::*;
use crate::{HandlerOutcome, ModuleStopHandle, RequestCtx, ServeOptions};
use async_trait::async_trait;
use serde_json::{json, Value};
use subc_protocol::{
    manifest::ModuleManifest, ModuleHelloAckBody, ModuleHelloBody, PROTOCOL_VERSION,
};
use subc_transport::{read_frame, write_frame};
use tokio::{
    io::DuplexStream,
    sync::mpsc,
    task::JoinHandle,
    time::{timeout, Duration},
};

#[derive(Debug)]
enum Seen {
    Applied(ConfigView),
    State(ConfigState),
    Files,
    Error(String),
    Draining,
}
struct Sample {
    seen: mpsc::UnboundedSender<Seen>,
    blocked: Option<Arc<Notify>>,
}
#[async_trait]
impl ModuleHandler for Sample {
    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
    fn config_declaration(&self) -> Option<ConfigDeclareRequest> {
        Some(ConfigDeclareRequest::new(
            json!({"type":"object"}),
            "/isolated/sample.json",
            vec![],
        ))
    }
    async fn config_apply(&self, view: ConfigView) -> Result<(), Vec<String>> {
        self.seen.send(Seen::Applied(view.clone())).unwrap();
        if let Some(blocked) = &self.blocked {
            blocked.notified().await;
        }
        if view.effective["value"] == json!(-1) {
            Err(vec!["negative value".into()])
        } else {
            Ok(())
        }
    }
    async fn config_review(&self, _: ConfigReviewRequest) -> ConfigReviewReply {
        serde_json::from_str(include_str!(
            "../../../subc-protocol/tests/golden/config_gate/config_review_reply.json"
        ))
        .unwrap()
    }
    async fn config_files(&self) {
        self.seen.send(Seen::Files).unwrap();
    }
    async fn on_config_state(&self, state: ConfigState) {
        self.seen.send(Seen::State(state)).unwrap();
    }
    async fn on_config_error(&self, error: ConfigError) {
        self.seen.send(Seen::Error(error.to_string())).unwrap();
    }
    async fn on_draining(&self, _: crate::RouteCloseReason, _: std::time::SystemTime) {
        self.seen.send(Seen::Draining).unwrap();
    }
}
struct Rig {
    wire: DuplexStream,
    task: JoinHandle<Result<(), crate::SubcModuleError>>,
    stop: ModuleStopHandle,
}
impl Rig {
    async fn new<H: ModuleHandler>(handler: H, gate: bool) -> (Self, Frame) {
        let (module, mut wire) = tokio::io::duplex(65536);
        let stop = ModuleStopHandle::new();
        let options = ServeOptions::default().with_stop_handle(stop.clone());
        let register = tokio::spawn(crate::register_connection(
            module,
            ModuleManifest::builder("sample", "1").build(),
            handler,
            options,
        ));
        let hello = recv(&mut wire).await;
        send(
            &mut wire,
            FrameType::HelloAck,
            hello.header.corr,
            serde_json::to_value(ModuleHelloAckBody {
                negotiated_ver: PROTOCOL_VERSION,
                subc_ops: vec![],
                subc_capabilities: if gate {
                    vec![CONFIG_GATE_CAPABILITY.into()]
                } else {
                    vec![]
                },
                storage: None,
                machine_id: None,
            })
            .unwrap(),
        )
        .await;
        let (_, serve) = register.await.unwrap().unwrap().unwrap();
        (
            Self {
                wire,
                task: tokio::spawn(serve),
                stop,
            },
            hello,
        )
    }
    async fn request(&mut self, op: &str) -> Frame {
        let frame = recv(&mut self.wire).await;
        assert_eq!(frame.header.ty, FrameType::Request);
        assert_eq!(body(&frame)["op"], op);
        frame
    }
    async fn ok(&mut self, request: Frame, value: Value) {
        send(
            &mut self.wire,
            FrameType::Response,
            request.header.corr,
            value,
        )
        .await;
    }
    async fn refuse(&mut self, request: Frame, code: &str) {
        send(
            &mut self.wire,
            FrameType::Error,
            request.header.corr,
            json!({"code":code,"message":"isolated refusal","detail":{"retained":true}}),
        )
        .await;
    }
    async fn hint(&mut self, generation: u64) {
        send(
            &mut self.wire,
            FrameType::Push,
            0,
            json!({"op":"config.changed","generation":generation,"fields":[]}),
        )
        .await;
    }
    async fn ping(&mut self) {
        send(&mut self.wire, FrameType::Ping, 800, Value::Null).await;
        let frame = recv(&mut self.wire).await;
        assert_eq!((frame.header.ty, frame.header.corr), (FrameType::Pong, 800));
    }
    async fn finish(mut self) {
        send(&mut self.wire, FrameType::Goodbye, 0, Value::Null).await;
        self.wire = tokio::io::duplex(1).0;
        timeout(Duration::from_secs(2), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
async fn recv(wire: &mut DuplexStream) -> Frame {
    timeout(Duration::from_secs(2), read_frame(wire))
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
async fn send(wire: &mut DuplexStream, ty: FrameType, corr: u64, body: Value) {
    let bytes = if matches!(ty, FrameType::Ping | FrameType::Goodbye) {
        vec![]
    } else {
        serde_json::to_vec(&body).unwrap()
    };
    write_frame(
        wire,
        &Frame::build(ty, crate::control_flags(), 0, 0, corr, bytes).unwrap(),
    )
    .await
    .unwrap();
}
fn body(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap()
}
async fn seen(rx: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
    timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
fn sample() -> (Sample, mpsc::UnboundedReceiver<Seen>) {
    let (seen, rx) = mpsc::unbounded_channel();
    (
        Sample {
            seen,
            blocked: None,
        },
        rx,
    )
}
fn view(generation: u64, status: ConfigStatus, value: i32) -> ConfigView {
    let mut view = ConfigView::new(
        generation,
        json!({"value":value}),
        Default::default(),
        vec![ConfigLayerSnapshot::file(
            "sample",
            ConfigLayer::User,
            "/isolated/sample.json",
            Some(format!("{{\"value\":{value}}}").into_bytes()),
        )],
        status,
    );
    view.attempt = (generation != 0).then_some(generation + 100);
    view
}
async fn declared(rig: &mut Rig) {
    let request = rig.request(CONFIG_DECLARE_OP).await;
    rig.ok(request, json!({"op":"config.declare"})).await;
}
async fn get(rig: &mut Rig, active: Option<ConfigView>, pending: Option<ConfigView>) {
    let request = rig.request(CONFIG_GET_OP).await;
    rig.ok(
        request,
        serde_json::to_value(ConfigGetReply::new(active, pending)).unwrap(),
    )
    .await;
}
async fn active(rx: &mut mpsc::UnboundedReceiver<Seen>, generation: u64) {
    loop {
        match seen(rx).await {
            Seen::State(ConfigState::Active(view)) => {
                assert_eq!(view.generation, generation);
                return;
            }
            Seen::Applied(_) | Seen::State(ConfigState::Pending(_)) => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

#[tokio::test]
async fn config_sdk_declares_fetches_applies_next_generation_and_nacks_without_losing_view() {
    let (handler, mut rx) = sample();
    let (mut rig, hello) = Rig::new(handler, true).await;
    let hello: ModuleHelloBody = serde_json::from_slice(&hello.body).unwrap();
    assert_eq!(
        hello.control_ops.unwrap(),
        vec!["health.check", "config.review"]
    );
    declared(&mut rig).await;
    let files = view(0, ConfigStatus::Unstamped, 1);
    get(&mut rig, Some(files.clone()), None).await;
    assert!(matches!(seen(&mut rx).await, Seen::Applied(view) if view == files));
    assert!(matches!(
        seen(&mut rx).await,
        Seen::State(ConfigState::Unstamped(_))
    ));
    let one = view(1, ConfigStatus::Pending, 2);
    rig.hint(1).await;
    get(&mut rig, None, Some(one.clone())).await;
    let ack = rig.request(CONFIG_APPLIED_OP).await;
    assert_eq!(
        body(&ack),
        json!({"op":"config.applied","generation":1,"attempt":101,"result":"ack","reasons":[]})
    );
    rig.ok(ack, json!({"op":"config.applied"})).await;
    active(&mut rx, 1).await;
    let mut one = one;
    one.status = ConfigStatus::Active;
    rig.hint(2).await;
    get(
        &mut rig,
        Some(one.clone()),
        Some(view(2, ConfigStatus::Pending, 3)),
    )
    .await;
    let ack = rig.request(CONFIG_APPLIED_OP).await;
    assert_eq!(body(&ack)["generation"], 2);
    rig.ok(ack, json!({"op":"config.applied"})).await;
    active(&mut rx, 2).await;
    rig.hint(3).await;
    get(
        &mut rig,
        Some(view(2, ConfigStatus::Active, 3)),
        Some(view(3, ConfigStatus::Pending, -1)),
    )
    .await;
    let nack = rig.request(CONFIG_APPLIED_OP).await;
    assert_eq!(body(&nack)["result"], "nack");
    assert_eq!(body(&nack)["reasons"], json!(["negative value"]));
    rig.ok(nack, json!({"op":"config.applied"})).await;
    assert!(matches!(seen(&mut rx).await, Seen::Applied(view) if view.generation == 3));
    assert!(
        matches!(seen(&mut rx).await, Seen::State(ConfigState::Failed { view: ConfigAvailability::LastView(view), .. }) if view.generation == 2)
    );
    rig.ping().await;
    rig.finish().await;
}

#[tokio::test]
async fn config_sdk_gated_declare_and_first_get_have_typed_no_approved_config_and_never_read_files()
{
    let (handler, mut rx) = sample();
    let (mut rig, _) = Rig::new(handler, true).await;
    let declare = rig.request(CONFIG_DECLARE_OP).await;
    rig.refuse(declare, "config_gated").await;
    assert!(
        matches!(seen(&mut rx).await, Seen::State(ConfigState::ConfigGated { view: ConfigAvailability::NoApprovedConfig, refusal }) if refusal.code == "config_gated")
    );
    rig.hint(1).await;
    let get = rig.request(CONFIG_GET_OP).await;
    rig.refuse(get, "config_gated").await;
    assert!(matches!(
        seen(&mut rx).await,
        Seen::State(ConfigState::ConfigGated {
            view: ConfigAvailability::NoApprovedConfig,
            ..
        })
    ));
    rig.ping().await;
    rig.finish().await;
}

#[tokio::test]
async fn config_sdk_retains_views_on_gated_and_not_delivery_target_and_retries_anchor_report_on_hint(
) {
    let (handler, mut rx) = sample();
    let (mut rig, _) = Rig::new(handler, true).await;
    declared(&mut rig).await;
    get(&mut rig, None, Some(view(1, ConfigStatus::Pending, 1))).await;
    let ack = rig.request(CONFIG_APPLIED_OP).await;
    let original_report = body(&ack);
    rig.refuse(ack, "anchor_pending").await;
    assert!(matches!(seen(&mut rx).await, Seen::Applied(_)));
    assert!(matches!(
        seen(&mut rx).await,
        Seen::State(ConfigState::Pending(_))
    ));
    assert!(
        matches!(seen(&mut rx).await, Seen::State(ConfigState::AnchorPending { view: ConfigAvailability::LastView(view), .. }) if view.generation == 1)
    );
    rig.hint(1).await;
    let ack = rig.request(CONFIG_APPLIED_OP).await;
    assert_eq!(body(&ack), original_report);
    rig.ok(ack, json!({"op":"config.applied"})).await;
    active(&mut rx, 1).await;
    let request = rig.request(CONFIG_GET_OP).await;
    rig.refuse(request, "config_gated").await;
    assert!(
        matches!(seen(&mut rx).await, Seen::State(ConfigState::ConfigGated { view: ConfigAvailability::LastView(view), .. }) if view.generation == 1)
    );
    rig.hint(2).await;
    get(
        &mut rig,
        Some(view(1, ConfigStatus::Active, 1)),
        Some(view(2, ConfigStatus::Pending, 2)),
    )
    .await;
    let ack = rig.request(CONFIG_APPLIED_OP).await;
    rig.refuse(ack, "config_gated").await;
    assert!(matches!(seen(&mut rx).await, Seen::Applied(_)));
    assert!(matches!(
        seen(&mut rx).await,
        Seen::State(ConfigState::Pending(_))
    ));
    assert!(
        matches!(seen(&mut rx).await, Seen::State(ConfigState::ConfigGated { view: ConfigAvailability::LastView(view), .. }) if view.generation == 2)
    );
    rig.hint(2).await;
    get(
        &mut rig,
        Some(view(1, ConfigStatus::Active, 1)),
        Some(view(2, ConfigStatus::Pending, 2)),
    )
    .await;
    let ack = rig.request(CONFIG_APPLIED_OP).await;
    rig.refuse(ack, "not_delivery_target").await;
    assert!(matches!(seen(&mut rx).await, Seen::Applied(_)));
    assert!(matches!(
        seen(&mut rx).await,
        Seen::State(ConfigState::Pending(_))
    ));
    assert!(
        matches!(seen(&mut rx).await, Seen::State(ConfigState::NotDeliveryTarget { view: ConfigAvailability::LastView(view), .. }) if view.generation == 2)
    );
    rig.ping().await;
    rig.hint(2).await;
    get(
        &mut rig,
        Some(view(1, ConfigStatus::Active, 1)),
        Some(view(2, ConfigStatus::Pending, 2)),
    )
    .await;
    let ack = rig.request(CONFIG_APPLIED_OP).await;
    rig.ok(ack, json!({"op":"config.applied"})).await;
    active(&mut rx, 2).await;
    rig.finish().await;
}

#[tokio::test]
async fn config_sdk_missing_capability_only_calls_existing_file_reader() {
    let (handler, mut rx) = sample();
    let (mut rig, _) = Rig::new(handler, false).await;
    assert!(matches!(seen(&mut rx).await, Seen::Files));
    rig.ping().await;
    rig.finish().await;
}

struct Unchanged;
#[async_trait]
impl ModuleHandler for Unchanged {
    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
}
#[tokio::test]
async fn config_sdk_unopted_handler_keeps_original_hello_bytes_and_sends_no_config_calls() {
    let (mut rig, hello) = Rig::new(Unchanged, true).await;
    let decoded: ModuleHelloBody = serde_json::from_slice(&hello.body).unwrap();
    let original = ModuleHelloBody {
        manifest: decoded.manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: Some(vec!["health.check".into()]),
        launch_nonce: decoded.launch_nonce,
    };
    assert_eq!(hello.body, serde_json::to_vec(&original).unwrap());
    rig.ping().await;
    rig.finish().await;
}

#[tokio::test]
async fn config_sdk_slow_apply_does_not_block_health_review_drain_or_stop() {
    let (mut handler, mut rx) = sample();
    let blocked = Arc::new(Notify::new());
    handler.blocked = Some(blocked);
    let (mut rig, _) = Rig::new(handler, true).await;
    declared(&mut rig).await;
    get(&mut rig, Some(view(0, ConfigStatus::Unstamped, 1)), None).await;
    assert!(matches!(seen(&mut rx).await, Seen::Applied(_)));
    send(
        &mut rig.wire,
        FrameType::Request,
        900,
        json!({"op":"health.check"}),
    )
    .await;
    let health = recv(&mut rig.wire).await;
    assert_eq!(health.header.corr, 900);
    assert_eq!(health.header.ty, FrameType::Response);
    send(
        &mut rig.wire,
        FrameType::Request,
        901,
        serde_json::to_value(ConfigReviewRequest::new(0, vec![], None)).unwrap(),
    )
    .await;
    let review = recv(&mut rig.wire).await;
    assert_eq!(review.header.corr, 901);
    assert_eq!(
        serde_json::from_slice::<ConfigReviewReply>(&review.body)
            .unwrap()
            .refusals[0]
            .code,
        "bound_violated"
    );
    send(
        &mut rig.wire,
        FrameType::Push,
        0,
        json!({"op":"module.draining","reason":"restart","deadline_ms":0}),
    )
    .await;
    assert!(matches!(seen(&mut rx).await, Seen::Draining));
    rig.stop.stop();
    assert_eq!(recv(&mut rig.wire).await.header.ty, FrameType::Goodbye);
    rig.finish().await;
}

#[tokio::test]
async fn config_sdk_unknown_status_is_not_applied_or_acknowledged() {
    let (handler, mut rx) = sample();
    let (mut rig, _) = Rig::new(handler, true).await;
    declared(&mut rig).await;
    get(
        &mut rig,
        Some(view(1, ConfigStatus::Unknown("future".into()), 2)),
        None,
    )
    .await;
    assert!(
        matches!(seen(&mut rx).await, Seen::Error(message) if message.contains("unknown status"))
    );
    rig.ping().await;
    rig.finish().await;
}

#[tokio::test]
async fn config_sdk_committed_wire_goldens_drive_pending_ack_and_gated_refusal() {
    let (handler, mut rx) = sample();
    let (mut rig, _) = Rig::new(handler, true).await;
    let declare = rig.request(CONFIG_DECLARE_OP).await;
    rig.ok(
        declare,
        serde_json::from_str(include_str!(
            "../../../subc-protocol/tests/golden/config_gate/config_declare_reply.json"
        ))
        .unwrap(),
    )
    .await;
    let get = rig.request(CONFIG_GET_OP).await;
    rig.ok(
        get,
        serde_json::from_str(include_str!(
            "../../../subc-protocol/tests/golden/config_gate/config_get_reply_pending.json"
        ))
        .unwrap(),
    )
    .await;
    let ack = rig.request(CONFIG_APPLIED_OP).await;
    let expected: Value = serde_json::from_str(include_str!(
        "../../../subc-protocol/tests/golden/config_gate/config_applied_request_ack.json"
    ))
    .unwrap();
    assert_eq!(body(&ack), expected);
    rig.ok(
        ack,
        serde_json::from_str(include_str!(
            "../../../subc-protocol/tests/golden/config_gate/config_applied_reply.json"
        ))
        .unwrap(),
    )
    .await;
    active(&mut rx, 3).await;
    active(&mut rx, 4).await;
    let review: Value = serde_json::from_str(include_str!(
        "../../../subc-protocol/tests/golden/config_gate/config_review_request.json"
    ))
    .unwrap();
    send(&mut rig.wire, FrameType::Request, 950, review).await;
    let reviewed = recv(&mut rig.wire).await;
    assert_eq!(reviewed.header.corr, 950);
    let expected: Value = serde_json::from_str(include_str!(
        "../../../subc-protocol/tests/golden/config_gate/config_review_reply.json"
    ))
    .unwrap();
    assert_eq!(body(&reviewed), expected);
    rig.hint(5).await;
    let get = rig.request(CONFIG_GET_OP).await;
    send(
        &mut rig.wire,
        FrameType::Error,
        get.header.corr,
        serde_json::from_str(include_str!(
            "../../../subc-protocol/tests/golden/config_gate/config_get_error_gated.json"
        ))
        .unwrap(),
    )
    .await;
    assert!(
        matches!(seen(&mut rx).await, Seen::State(ConfigState::ConfigGated { view: ConfigAvailability::LastView(view), refusal }) if view.generation == 4 && refusal.code == "config_gated")
    );
    rig.ping().await;
    rig.finish().await;
}

#[tokio::test]
async fn config_sdk_first_pending_nack_keeps_the_supplied_active_recovery_view() {
    let (handler, mut rx) = sample();
    let (mut rig, _) = Rig::new(handler, true).await;
    declared(&mut rig).await;
    get(
        &mut rig,
        Some(view(1, ConfigStatus::Active, 1)),
        Some(view(2, ConfigStatus::Pending, -1)),
    )
    .await;
    let nack = rig.request(CONFIG_APPLIED_OP).await;
    assert_eq!(body(&nack)["result"], "nack");
    rig.ok(nack, json!({"op":"config.applied"})).await;
    active(&mut rx, 1).await;
    assert!(matches!(seen(&mut rx).await, Seen::Applied(view) if view.generation == 2));
    assert!(
        matches!(seen(&mut rx).await, Seen::State(ConfigState::Failed { view: ConfigAvailability::LastView(view), .. }) if view.generation == 1)
    );
    rig.finish().await;
}

#[tokio::test]
async fn config_sdk_project_hint_fetches_and_reports_only_that_project() {
    let (handler, mut rx) = sample();
    let (mut rig, _) = Rig::new(handler, true).await;
    declared(&mut rig).await;
    get(&mut rig, Some(view(0, ConfigStatus::Unstamped, 1)), None).await;
    assert!(matches!(seen(&mut rx).await, Seen::Applied(_)));
    assert!(matches!(
        seen(&mut rx).await,
        Seen::State(ConfigState::Unstamped(_))
    ));
    send(
        &mut rig.wire,
        FrameType::Push,
        0,
        json!({"op":"config.changed", "generation":2,"project_id":"p","fields":[]}),
    )
    .await;
    let request = rig.request(CONFIG_GET_OP).await;
    assert_eq!(body(&request)["project_id"], "p");
    let mut pending = view(2, ConfigStatus::Pending, 2);
    pending.project_id = Some("p".into());
    rig.ok(
        request,
        serde_json::to_value(ConfigGetReply::new(None, Some(pending))).unwrap(),
    )
    .await;
    let ack = rig.request(CONFIG_APPLIED_OP).await;
    assert_eq!(body(&ack)["project_id"], "p");
    rig.ok(ack, json!({"op":"config.applied"})).await;
    active(&mut rx, 2).await;
    rig.finish().await;
}
