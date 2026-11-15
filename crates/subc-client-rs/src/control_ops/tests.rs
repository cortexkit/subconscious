use super::*;
use serde_json::{json, Value};
use subc_protocol::consent::{ConsentDecision, ConsentRequest, ConsentResponse, CONSENT_V1};
use tokio::{io::DuplexStream, sync::Notify};

const WAIT: Duration = Duration::from_secs(2);

struct Handler {
    entered: mpsc::UnboundedSender<(String, Value, CancellationToken)>,
    release: Arc<Notify>,
    declarations: Arc<AtomicU64>,
    config: bool,
}

#[async_trait]
impl ModuleHandler for Handler {
    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }

    fn control_ops(&self) -> Vec<String> {
        self.declarations.fetch_add(1, Ordering::SeqCst);
        vec!["sample.check".into(), CONSENT_V1.into()]
    }

    fn config_declaration(&self) -> Option<config::ConfigDeclareRequest> {
        self.config.then(|| {
            config::ConfigDeclareRequest::new(
                json!({"type":"object"}),
                "/isolated/sample.json",
                vec![],
            )
        })
    }

    async fn config_review(&self, _: config::ConfigReviewRequest) -> config::ConfigReviewReply {
        config::ConfigReviewReply::new(vec![], vec![])
    }

    async fn on_control_request(
        &self,
        op: &str,
        body: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ErrorBody> {
        self.entered
            .send((op.into(), body.clone(), cancel))
            .unwrap();
        if body["wait"] == true {
            self.release.notified().await;
        }
        if body["fail"] == true {
            return Err(ErrorBody::new(
                "approval_surface_unavailable",
                "phone is offline",
            ));
        }
        if op == CONSENT_V1 {
            let request: ConsentRequest = serde_json::from_value(body).unwrap();
            assert!(request.is_consistent());
            return Ok(serde_json::to_value(ConsentResponse::new(
                &request,
                ConsentDecision::Approved,
            ))
            .unwrap());
        }
        Ok(json!({"op":op,"answer":42}))
    }
}

struct Rig {
    wire: DuplexStream,
    serving: tokio::task::JoinHandle<Result<(), SubcModuleError>>,
    entered: mpsc::UnboundedReceiver<(String, Value, CancellationToken)>,
    release: Arc<Notify>,
    declarations: Arc<AtomicU64>,
    hello: ModuleHelloBody,
}

impl Rig {
    async fn new() -> Self {
        Self::with_config(false).await
    }

    async fn with_config(config: bool) -> Self {
        let (module, mut wire) = tokio::io::duplex(65536);
        let (entered, rx) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let declarations = Arc::new(AtomicU64::new(0));
        let register = tokio::spawn(register_connection(
            module,
            ModuleManifest::builder("sample", "1.0.0").build(),
            Handler {
                entered,
                release: release.clone(),
                declarations: declarations.clone(),
                config,
            },
            ServeOptions::default(),
        ));
        let hello = recv(&mut wire).await;
        let hello_body = serde_json::from_slice(&hello.body).unwrap();
        send(
            &mut wire,
            Frame::build(
                FrameType::HelloAck,
                control_flags(),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&ModuleHelloAckBody {
                    negotiated_ver: PROTOCOL_VERSION,
                    subc_ops: vec![],
                    subc_capabilities: if config {
                        vec![config::CONFIG_GATE_CAPABILITY.into()]
                    } else {
                        vec![]
                    },
                    storage: None,
                    machine_id: None,
                })
                .unwrap(),
            )
            .unwrap(),
        )
        .await;
        let (_handle, serving) = register.await.unwrap().unwrap().unwrap();
        let mut rig = Self {
            wire,
            serving: tokio::spawn(serving),
            entered: rx,
            release,
            declarations,
            hello: hello_body,
        };
        if config {
            let declare = recv(&mut rig.wire).await;
            assert_eq!(
                serde_json::from_slice::<Value>(&declare.body).unwrap()["op"],
                "config.declare"
            );
            // The declaration refusal stops background fetches; review itself
            // remains available for the snapshots supplied by the daemon.
            send(
                &mut rig.wire,
                control(
                    FrameType::Error,
                    declare.header.corr,
                    serde_json::to_vec(&ErrorBody::new("unavailable", "isolated test")).unwrap(),
                ),
            )
            .await;
        }
        rig
    }

    async fn request(&mut self, corr: u64, body: Value) {
        send(
            &mut self.wire,
            control(FrameType::Request, corr, serde_json::to_vec(&body).unwrap()),
        )
        .await;
    }

    async fn entered(&mut self) -> (String, Value, CancellationToken) {
        timeout(WAIT, self.entered.recv())
            .await
            .expect("control handler did not start")
            .unwrap()
    }

    async fn finish(self) {
        self.release.notify_waiters();
        drop(self.wire);
        timeout(WAIT, self.serving).await.unwrap().unwrap().unwrap();
    }
}

fn control(ty: FrameType, corr: u64, body: Vec<u8>) -> Frame {
    Frame::build(ty, control_flags(), 0, 0, corr, body).unwrap()
}

async fn send(wire: &mut DuplexStream, frame: Frame) {
    write_frame(wire, &frame).await.unwrap();
}

async fn recv(wire: &mut DuplexStream) -> Frame {
    timeout(WAIT, read_frame(wire))
        .await
        .expect("no reply from module")
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn declared_op_is_advertised_once_in_hello() {
    let rig = Rig::new().await;
    assert_eq!(
        rig.hello.control_ops.as_ref().unwrap(),
        &["health.check", "sample.check", CONSENT_V1]
    );
    assert_eq!(rig.declarations.load(Ordering::SeqCst), 1);
    rig.finish().await;
}

#[tokio::test]
async fn declared_request_reaches_handler_and_reply_keeps_correlation() {
    let mut rig = Rig::new().await;
    let body = json!({"op":"sample.check","input":{"number":7}});
    rig.request(120, body.clone()).await;
    let (op, received, _) = rig.entered().await;
    assert_eq!(op, "sample.check");
    assert_eq!(received, body);
    let frame = recv(&mut rig.wire).await;
    assert_eq!(
        (frame.header.ty, frame.header.channel, frame.header.corr),
        (FrameType::Response, 0, 120)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&frame.body).unwrap(),
        json!({"op":"sample.check","answer":42})
    );
    assert_eq!(rig.declarations.load(Ordering::SeqCst), 1);
    rig.finish().await;
}

#[tokio::test]
async fn consent_request_uses_exact_daemon_serialization_without_op() {
    let request: ConsentRequest = serde_json::from_str(include_str!(
        "../../../subc-protocol/tests/golden/config_gate/consent_v1_request.json"
    ))
    .unwrap();
    let bytes = serde_json::to_vec(&request).unwrap();
    assert!(serde_json::from_slice::<Value>(&bytes)
        .unwrap()
        .get("op")
        .is_none());
    let mut rig = Rig::new().await;
    send(&mut rig.wire, control(FrameType::Request, 121, bytes)).await;
    let (op, _, _) = rig.entered().await;
    assert_eq!(op, CONSENT_V1);
    let frame = recv(&mut rig.wire).await;
    assert_eq!(
        (frame.header.ty, frame.header.corr),
        (FrameType::Response, 121)
    );
    assert!(serde_json::from_slice::<ConsentResponse>(&frame.body)
        .unwrap()
        .approves(&request));
    rig.finish().await;
}

#[tokio::test]
async fn typed_control_error_reaches_daemon_with_code() {
    let mut rig = Rig::new().await;
    rig.request(122, json!({"op":"sample.check","fail":true}))
        .await;
    rig.entered().await;
    let frame = recv(&mut rig.wire).await;
    assert_eq!(
        (frame.header.ty, frame.header.channel, frame.header.corr),
        (FrameType::Error, 0, 122)
    );
    let error: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(
        (error.code.as_str(), error.message.as_str()),
        ("approval_surface_unavailable", "phone is offline")
    );
    rig.finish().await;
}

#[tokio::test]
async fn long_control_handler_does_not_block_health_route_or_config_review() {
    let mut rig = Rig::with_config(true).await;
    rig.request(130, json!({"op":"sample.check","wait":true}))
        .await;
    rig.entered().await;
    rig.request(131, json!({"op":"health.check"})).await;
    let health = recv(&mut rig.wire).await;
    assert_eq!(
        (health.header.ty, health.header.corr),
        (FrameType::Response, 131)
    );
    rig.request(
        132,
        serde_json::to_value(ModuleControlRequest::RouteBind {
            route_channel: 1,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "sample".into(),
            },
            identity: BindIdentity::new("test", "bind"),
            principal: None,
            consumer_capabilities: None,
            role_versions: None,
            admission_facts: None,
            scope: None,
            call_mode: None,
        })
        .unwrap(),
    )
    .await;
    let bind = recv(&mut rig.wire).await;
    assert_eq!(
        (bind.header.ty, bind.header.corr),
        (FrameType::Response, 132)
    );
    send(
        &mut rig.wire,
        Frame::build(
            FrameType::Request,
            data_flags(),
            1,
            1,
            133,
            b"route request".to_vec(),
        )
        .unwrap(),
    )
    .await;
    let route = recv(&mut rig.wire).await;
    assert_eq!(
        (route.header.ty, route.header.channel, route.header.corr),
        (FrameType::Response, 1, 133)
    );
    assert_eq!(route.body, b"route request");
    rig.request(
        134,
        serde_json::to_value(config::ConfigReviewRequest::new(0, vec![], None)).unwrap(),
    )
    .await;
    let review = recv(&mut rig.wire).await;
    assert_eq!(
        (review.header.ty, review.header.corr),
        (FrameType::Response, 134)
    );
    assert!(
        serde_json::from_slice::<config::ConfigReviewReply>(&review.body)
            .unwrap()
            .refusals
            .is_empty()
    );
    rig.release.notify_one();
    assert_eq!(recv(&mut rig.wire).await.header.corr, 130);
    rig.finish().await;
}

#[tokio::test]
async fn cancel_signals_handler_and_drops_its_late_reply() {
    let (entered, mut rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let handler = Arc::new(Handler {
        entered,
        release: release.clone(),
        declarations: Arc::new(AtomicU64::new(0)),
        config: false,
    });
    let (writer, mut outbound) = mpsc::channel(1);
    let handle = ModuleHandle::new(
        &ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: vec![],
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        },
        writer,
        1,
        CancellationToken::new(),
        tokio::runtime::Handle::current(),
    );
    handle
        .shared
        .served_control_ops
        .set(vec!["sample.check".into()])
        .unwrap();
    let (egress, mut replies) = mpsc::channel(1);
    let dispatcher = RequestDispatcher::new();
    handle_frame(
        control(
            FrameType::Request,
            140,
            br#"{"op":"sample.check","wait":true}"#.to_vec(),
        ),
        &egress,
        handler.clone(),
        dispatcher.clone(),
        handle.clone(),
    )
    .await
    .unwrap();
    let (_, _, cancel) = timeout(WAIT, rx.recv())
        .await
        .expect("control handler did not start")
        .unwrap();
    handle_frame(
        control(FrameType::Cancel, 140, vec![]),
        &egress,
        handler,
        dispatcher.clone(),
        handle,
    )
    .await
    .unwrap();
    timeout(WAIT, cancel.cancelled())
        .await
        .expect("CANCEL did not signal handler");
    release.notify_one();
    dispatcher.tasks.close();
    timeout(WAIT, dispatcher.tasks.wait())
        .await
        .expect("late handler did not finish");
    assert!(matches!(
        replies.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(outbound.try_recv().is_err());
    assert!(lock_in_flight(&dispatcher.in_flight).unwrap().is_empty());
}

#[tokio::test]
async fn disconnect_signals_every_running_control_handler() {
    let mut rig = Rig::new().await;
    let mut tokens = Vec::new();
    for corr in 150..153 {
        rig.request(corr, json!({"op":"sample.check","wait":true}))
            .await;
        tokens.push(rig.entered().await.2);
    }
    drop(rig.wire);
    for token in tokens {
        timeout(WAIT, token.cancelled())
            .await
            .expect("disconnect did not signal handler");
    }
    rig.release.notify_waiters();
    timeout(WAIT, rig.serving).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn control_concurrency_is_bounded_and_overload_does_not_block_health() {
    let mut rig = Rig::new().await;
    for corr in 200..216 {
        rig.request(corr, json!({"op":"sample.check","wait":true}))
            .await;
        rig.entered().await;
    }
    rig.request(216, json!({"op":"sample.check"})).await;
    let refused = recv(&mut rig.wire).await;
    assert_eq!(
        (refused.header.ty, refused.header.corr),
        (FrameType::Error, 216)
    );
    assert_eq!(
        serde_json::from_slice::<ErrorBody>(&refused.body)
            .unwrap()
            .code,
        "resource_exhausted"
    );
    assert!(rig.entered.try_recv().is_err());
    rig.request(217, json!({"op":"health.check"})).await;
    assert_eq!(recv(&mut rig.wire).await.header.corr, 217);
    rig.release.notify_waiters();
    for _ in 0..16 {
        assert!((200..216).contains(&recv(&mut rig.wire).await.header.corr));
    }
    rig.request(218, json!({"op":"sample.check"})).await;
    assert_eq!(recv(&mut rig.wire).await.header.corr, 218);
    rig.finish().await;
}

#[tokio::test]
async fn op_takes_precedence_and_nonstring_op_does_not_fall_back_to_protocol() {
    let mut rig = Rig::new().await;
    for (corr, op) in [(230, json!("unknown.op")), (231, Value::Null)] {
        rig.request(corr, json!({"op":op,"protocol":"sample.check"}))
            .await;
        let refusal = recv(&mut rig.wire).await;
        assert_eq!(
            (refusal.header.ty, refusal.header.corr),
            (FrameType::Error, corr)
        );
        assert_eq!(
            serde_json::from_slice::<ErrorBody>(&refusal.body)
                .unwrap()
                .code,
            "invalid_request"
        );
    }
    assert!(rig.entered.try_recv().is_err());
    rig.finish().await;
}

#[test]
fn invalid_reserved_and_duplicate_control_names_are_refused() {
    for op in [
        "",
        "health.check",
        "config.review",
        "route.bind",
        "has space",
        "bad\nname",
        "é",
        "a..b",
        "/a",
        "a/",
        "a:b",
    ] {
        assert!(
            matches!(
                validate(vec![op.into()]),
                Err(SubcModuleError::InvalidControlOp(_))
            ),
            "accepted {op:?}"
        );
    }
    assert!(validate(vec!["sample.check".into(), "sample.check".into()]).is_err());
    assert!(validate(vec![
        "sample.check".into(),
        "consent/v1".into(),
        "custom_op-v2".into()
    ])
    .is_ok());
}

#[tokio::test]
async fn invalid_declaration_fails_before_any_hello_is_written() {
    struct Invalid;
    #[async_trait]
    impl ModuleHandler for Invalid {
        async fn handle(&self, _: RequestCtx, _: Vec<u8>) -> HandlerOutcome {
            unreachable!()
        }
        fn control_ops(&self) -> Vec<String> {
            vec!["config.review".into()]
        }
    }
    let (module, mut wire) = tokio::io::duplex(1024);
    let result = register_connection(
        module,
        ModuleManifest::builder("sample", "1").build(),
        Invalid,
        ServeOptions::default(),
    )
    .await;
    assert!(matches!(result, Err(SubcModuleError::InvalidControlOp(op)) if op == "config.review"));
    assert!(read_frame(&mut wire).await.unwrap().is_none());
}
