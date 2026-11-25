use super::*;
use config::{
    ConfigDeclareRequest, ConfigGetReply, ConfigStatus, ConfigView, CONFIG_GATE_CAPABILITY,
};
use serde_json::json;
use subc_transport::{
    authenticate_server,
    connection_file::{ConnectionInfo, Endpoint, LocalEndpoint, SCHEMA_VERSION},
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Notify,
};

const BUDGET: Duration = Duration::from_secs(4);
const CATALOG: &str = "catalog.update";

#[test]
fn live_upgrade_capability_requires_opted_in_primary() {
    let default = ServeOptions {
        registration_channel: true,
        ..ServeOptions::default()
    };
    assert!(!advertises_upgrade(&default, false));
    let primary = default.with_live_upgrade(true);
    assert!(advertises_upgrade(&primary, false));
    let tcp = ServeOptions::default().with_live_upgrade(true);
    assert!(advertises_upgrade(&tcp, true));
    assert!(!advertises_upgrade(&tcp, false));
    let prefix = ServeOptions {
        prefix_child: true,
        ..tcp
    };
    assert!(!advertises_upgrade(&prefix, true));
}

#[derive(Default)]
struct Observed {
    data: Mutex<Option<CancellationToken>>,
    control: Mutex<Option<CancellationToken>>,
    data_started: Notify,
    data_release: Notify,
    data_finished: Notify,
    retired_started: Notify,
    retired_cancelled: AtomicBool,
    control_started: Notify,
    control_dropped: Notify,
    old_config_started: Notify,
    old_config_dropped: Notify,
    new_config_applied: Notify,
    reconnected: Notify,
    ends: Mutex<Vec<ConnectionEnd>>,
    ack_ran: AtomicBool,
}

struct Cancelled<'a>(&'a Notify);
impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

struct Probe(Arc<Observed>);
#[async_trait]
impl ModuleHandler for Probe {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if body == b"hold-until-final-stop" {
            self.0.retired_started.notify_one();
            ctx.cancelled().await;
            self.0.retired_cancelled.store(true, Ordering::Release);
            return HandlerOutcome::Response(body);
        }
        *self.0.data.lock().unwrap() = Some(ctx.cancellation_token());
        self.0.data_started.notify_one();
        self.0.data_release.notified().await;
        assert!(!ctx.cancellation_token().is_cancelled());
        self.0.data_finished.notify_one();
        HandlerOutcome::Response(body)
    }

    fn config_declaration(&self) -> Option<ConfigDeclareRequest> {
        Some(ConfigDeclareRequest::new(
            json!({"type":"object"}),
            "/isolated/upgrade.json",
            vec![],
        ))
    }

    async fn config_apply(&self, view: ConfigView) -> Result<(), Vec<String>> {
        if view.generation == 1 {
            let _dropped = Cancelled(&self.0.old_config_dropped);
            self.0.old_config_started.notify_one();
            std::future::pending().await
        } else {
            assert_eq!(view.generation, 2);
            self.0.new_config_applied.notify_one();
            Ok(())
        }
    }

    fn control_ops(&self) -> Vec<String> {
        vec!["test.block".into()]
    }

    async fn on_control_request(
        &self,
        op: &str,
        _: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value, ErrorBody> {
        assert_eq!(op, "test.block");
        *self.0.control.lock().unwrap() = Some(cancel);
        let _dropped = Cancelled(&self.0.control_dropped);
        self.0.control_started.notify_one();
        std::future::pending().await
    }

    async fn resume_scope_sync(&self, handle: &ModuleHandle) -> Option<ResumeScopeSync> {
        assert!(handle.daemon_supports("new-capability"));
        assert_eq!(handle.storage(), Some(&json!({"generation":2})));
        // The hook can await while building the snapshot; no reader or config
        // work may send around it.
        tokio::task::yield_now().await;
        Some(ResumeScopeSync {
            generation: 43,
            scopes: vec![],
        })
    }

    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        assert!(ack.subc_capabilities.iter().any(|v| v == "new-capability"));
        self.0.ack_ran.store(true, Ordering::Release);
    }

    async fn on_reconnected(
        &self,
        handle: &ModuleHandle,
        sync: Option<Result<ScopeSyncReply, ScopeCallError>>,
    ) {
        assert!(self.0.ack_ran.load(Ordering::Acquire));
        assert_eq!(
            sync.unwrap().unwrap_err().code(),
            Some("scope_sync_not_authority")
        );
        handle.set_ready(true).await.unwrap();
        self.0.new_config_applied.notified().await;
        self.0.reconnected.notify_one();
    }

    async fn on_connection_end(&self, end: ConnectionEnd) {
        self.0.ends.lock().unwrap().push(end);
    }
}

struct Local {
    _scratch: cortexkit_test_support::ScratchDir,
    file: PathBuf,
    listener: UnixListener,
    info: ConnectionInfo,
}

impl Local {
    fn new() -> Self {
        let scratch = cortexkit_test_support::ScratchDir::new("live-upgrade");
        let file = scratch.path().join("connection.json");
        let socket = scratch.path().join("local.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let info = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: 1,
            }],
            key: vec![34; 32],
            daemon_id: [56; 16],
            pid: std::process::id(),
            daemon_ver: "1".into(),
        };
        connection_file::write_atomic_with_local_endpoint(
            &file,
            &info,
            Some(&LocalEndpoint::Unix { path: socket }),
        )
        .unwrap();
        Self {
            _scratch: scratch,
            file,
            listener,
            info,
        }
    }

    async fn hello(&self) -> (UnixStream, ModuleHelloBody) {
        let (mut stream, _) = timeout(BUDGET, self.listener.accept())
            .await
            .unwrap()
            .unwrap();
        authenticate_server(
            &mut stream,
            &self.info.key,
            &self.info.daemon_id,
            &self.info.daemon_ver,
            BUDGET,
        )
        .await
        .unwrap();
        let hello = recv(&mut stream).await;
        assert_eq!(hello.header.ty, FrameType::Hello);
        (stream, serde_json::from_slice(&hello.body).unwrap())
    }
}

fn ack(capability: &str, generation: u64) -> ModuleHelloAckBody {
    ModuleHelloAckBody {
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: vec![CATALOG.into(), SCOPE_SYNC_OP.into(), "claim.mint".into()],
        subc_capabilities: vec![CONFIG_GATE_CAPABILITY.into(), capability.into()],
        storage: Some(json!({"generation":generation})),
        machine_id: None,
        resume_token: Some(format!("fixture-token-{generation}")),
    }
}

fn configure(handle: &ModuleHandle, ack: &ModuleHelloAckBody, handler: &Probe) {
    let shared = handle.shared();
    let manifest = ModuleManifest::builder("test-module", "1")
        .ready(true)
        .build();
    let mut state = shared.lock_inner();
    state.current_provides = Some(manifest.provides.clone());
    state.upgrade.manifest = Some(manifest);
    state.upgrade.resume_token = ack.resume_token.clone();
    let _ = shared.served_control_ops.set(handler.control_ops());
    let _ = shared
        .config
        .declaration
        .set(handler.config_declaration().unwrap());
    registration::rebind_owner(&shared);
}

async fn send<S: AsyncWrite + Unpin>(
    stream: &mut S,
    ty: FrameType,
    corr: u64,
    body: impl serde::Serialize,
) {
    let frame = Frame::build(
        ty,
        control_flags(),
        0,
        0,
        corr,
        if matches!(ty, FrameType::Ping | FrameType::Goodbye) {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        },
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
}

async fn recv<S: AsyncRead + Unpin>(stream: &mut S) -> Frame {
    timeout(BUDGET, read_frame(stream))
        .await
        .unwrap()
        .unwrap()
        .expect("frame before EOF")
}

fn operation(frame: &Frame) -> String {
    assert_eq!(frame.header.ty, FrameType::Request);
    serde_json::from_slice::<serde_json::Value>(&frame.body).unwrap()["op"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn ping<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, correlation: u64) {
    send(stream, FrameType::Ping, correlation, ()).await;
    let pong = recv(stream).await;
    assert_eq!(
        (pong.header.ty, pong.header.corr),
        (FrameType::Pong, correlation)
    );
}

fn view(generation: u64) -> ConfigGetReply {
    ConfigGetReply::new(
        Some(ConfigView::new(
            generation,
            json!({}),
            Default::default(),
            vec![],
            ConfigStatus::Active,
        )),
        None,
    )
}

async fn wait(signal: &Notify) {
    timeout(BUDGET, signal.notified())
        .await
        .expect("handler did not reach expected step");
}

#[tokio::test]
async fn live_upgrade_fences_new_calls_and_resumes_with_scope_first() {
    let local = Local::new();
    let observed = Arc::new(Observed::default());
    let handler = Arc::new(Probe(observed.clone()));
    let (module, mut daemon) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(module);
    let (tx, rx) = mpsc::channel(EGRESS_BUFFER);
    let initial = ack("old-capability", 1);
    let handle = ModuleHandle::new(
        &initial,
        tx.clone(),
        u64::MAX - 1,
        CancellationToken::new(),
        tokio::runtime::Handle::current(),
    );
    configure(&handle, &initial, &handler);
    let old_route = handle.route_handle(1, 1);
    handle.install_route(old_route).unwrap();
    let old_caps = handle.daemon_capabilities();
    let old_storage = handle.storage();
    let old_closed = handle.closed();
    let options = ServeOptions {
        connection_file: Some(local.file.clone()),
        ..ServeOptions::default().with_live_upgrade(true)
    };
    let serving = tokio::spawn(connection_serve_future_with_options(
        reader,
        tx,
        handler,
        handle.clone(),
        tokio::spawn(drain_writer(writer, rx)),
        options,
        None,
    ));

    let declare = recv(&mut daemon).await;
    assert_eq!(operation(&declare), "config.declare");
    send(
        &mut daemon,
        FrameType::Response,
        declare.header.corr,
        json!({"op":"config.declare"}),
    )
    .await;
    let get = recv(&mut daemon).await;
    assert_eq!(operation(&get), "config.get");
    send(&mut daemon, FrameType::Response, get.header.corr, view(1)).await;
    wait(&observed.old_config_started).await;

    let data = Frame::build(
        FrameType::Request,
        data_flags(),
        1,
        1,
        90,
        b"stale-data-reply".to_vec(),
    )
    .unwrap();
    write_frame(&mut daemon, &data).await.unwrap();
    wait(&observed.data_started).await;
    let retained = Frame::build(
        FrameType::Request,
        data_flags(),
        1,
        1,
        89,
        b"hold-until-final-stop".to_vec(),
    )
    .unwrap();
    write_frame(&mut daemon, &retained).await.unwrap();
    wait(&observed.retired_started).await;
    send(
        &mut daemon,
        FrameType::Request,
        91,
        json!({"op":"test.block"}),
    )
    .await;
    wait(&observed.control_started).await;
    let role = ProviderRole::ToolProvider {
        tools: vec![],
        identity_scope: vec![],
        concurrency: Default::default(),
        emits_push: false,
        sub_supervises: false,
    };
    let update = tokio::spawn({
        let handle = handle.clone();
        let role = role.clone();
        async move { handle.catalog_update(vec![role]).await }
    });
    let update_request = recv(&mut daemon).await;
    assert_eq!(operation(&update_request), CATALOG);
    send(
        &mut daemon,
        FrameType::Response,
        update_request.header.corr,
        json!({"op":CATALOG}),
    )
    .await;
    update.await.unwrap().unwrap();
    let ready = tokio::spawn({
        let handle = handle.clone();
        async move { handle.set_ready(false).await }
    });
    let request = recv(&mut daemon).await;
    assert_eq!(operation(&request), CATALOG);

    let reconnect_by_ms =
        (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() + BUDGET).as_millis() as u64;
    send(
        &mut daemon,
        FrameType::Push,
        0,
        json!({"op":"module.upgrading","reconnect_by_ms":reconnect_by_ms}),
    )
    .await;
    ping(&mut daemon, 93).await;
    assert!(!handle.refresh_config());
    assert!(!handle.refresh_project_config("test-project"));
    match handle.catalog_update(vec![]).await.unwrap_err() {
        CatalogUpdateError::Rejected(body) => {
            assert_eq!(body.code, error_codes::MODULE_RELOADING);
            assert_eq!(body.detail, Some(json!({"reason":"daemon_upgrade"})));
        }
        other => panic!("expected a pre-send refusal, got {other:?}"),
    }
    assert_eq!(
        handle.scope_sync(3, vec![]).await.unwrap_err().code(),
        Some(error_codes::MODULE_RELOADING)
    );
    assert!(
        matches!(handle.helper_claim_handoff().await, Err(registration::ClaimError::Refused { code }) if code == error_codes::MODULE_RELOADING)
    );
    // The next and only wire frame is the probe, not one of the fenced RPCs.
    ping(&mut daemon, 94).await;
    send(
        &mut daemon,
        FrameType::Response,
        request.header.corr,
        json!({"op":CATALOG}),
    )
    .await;
    send(&mut daemon, FrameType::Goodbye, 0, ()).await;
    ready.await.unwrap().unwrap();
    timeout(BUDGET, old_closed).await.unwrap();
    wait(&observed.control_dropped).await;
    wait(&observed.old_config_dropped).await;
    assert!(observed
        .control
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_cancelled());
    assert!(!observed
        .data
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_cancelled());
    assert!(observed.ends.lock().unwrap().is_empty());

    let (mut resumed, hello) = local.hello().await;
    assert_eq!(hello.resume_token.as_deref(), Some("fixture-token-1"));
    assert!(hello.launch_nonce.is_none());
    assert_eq!(hello.manifest.ready, Some(false));
    assert_eq!(hello.manifest.provides, vec![role]);
    let resumed_ack = ack("new-capability", 2);
    let frames = [
        Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            HELLO_CORR,
            serde_json::to_vec(&resumed_ack).unwrap(),
        )
        .unwrap(),
        Frame::build(FrameType::Ping, control_flags(), 0, 0, 95, Vec::new()).unwrap(),
    ];
    for frame in frames {
        write_frame(&mut resumed, &frame).await.unwrap();
    }
    let first = recv(&mut resumed).await;
    assert_eq!(operation(&first), SCOPE_SYNC_OP);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first.body).unwrap()["generation"],
        43
    );
    send(
        &mut resumed,
        FrameType::Error,
        first.header.corr,
        ErrorBody::new("scope_sync_not_authority", "test refusal"),
    )
    .await;
    let (mut pong, mut config, mut catalog) = (false, false, false);
    while !(pong && config && catalog) {
        let frame = recv(&mut resumed).await;
        if frame.header.ty == FrameType::Pong {
            assert_eq!(frame.header.corr, 95);
            pong = true;
            continue;
        }
        let op = operation(&frame);
        match op.as_str() {
            "config.declare" | CATALOG => {
                send(
                    &mut resumed,
                    FrameType::Response,
                    frame.header.corr,
                    json!({"op":op}),
                )
                .await;
                catalog |= op == CATALOG;
            }
            "config.get" => {
                send(
                    &mut resumed,
                    FrameType::Response,
                    frame.header.corr,
                    view(2),
                )
                .await;
                config = true;
            }
            other => panic!("unexpected operation {other}"),
        }
    }
    wait(&observed.reconnected).await;
    assert!(!handle.is_closed());
    assert!(handle.daemon_supports("new-capability"));
    assert_eq!(
        old_caps,
        &[
            CONFIG_GATE_CAPABILITY.to_owned(),
            "old-capability".to_owned()
        ]
    );
    assert_eq!(old_storage, Some(&json!({"generation":1})));
    assert!(observed.ends.lock().unwrap().is_empty());
    assert!(!observed.retired_cancelled.load(Ordering::Acquire));
    assert!(matches!(
        handle.push(&old_route, vec![], None).await,
        Err(SubcModuleError::StaleRouteHandle(_))
    ));
    observed.data_release.notify_one();
    wait(&observed.data_finished).await;
    ping(&mut resumed, 96).await;
    assert!(
        timeout(Duration::from_millis(25), read_frame(&mut resumed))
            .await
            .is_err(),
        "a stale reply reached the new connection"
    );
    drop(resumed);
    timeout(BUDGET, serving).await.unwrap().unwrap().unwrap();
    assert_eq!(*observed.ends.lock().unwrap(), [ConnectionEnd::Eof]);
    assert!(observed.retired_cancelled.load(Ordering::Acquire));
}

async fn reconnect_without_ack(refusal: Option<&str>) {
    let local = Local::new();
    let initial = ack("old-capability", 1);
    let (tx, _rx) = mpsc::channel(EGRESS_BUFFER);
    let handle = ModuleHandle::new(
        &initial,
        tx,
        1,
        CancellationToken::new(),
        tokio::runtime::Handle::current(),
    );
    configure(&handle, &initial, &Probe(Arc::new(Observed::default())));
    let deadline = Instant::now() + BUDGET;
    let task = tokio::spawn({
        let path = local.file.clone();
        async move { reconnect(&path, deadline, &handle.shared(), &ModuleStopHandle::new()).await }
    });
    let (mut socket, _) = local.hello().await;
    if let Some(code) = refusal {
        send(
            &mut socket,
            FrameType::Error,
            HELLO_CORR,
            ErrorBody::new(code, "refused"),
        )
        .await;
        assert!(timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_none());
    } else {
        assert!(timeout_at(deadline + Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        assert!(Instant::now() >= deadline);
        assert!(read_frame(&mut socket).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn live_upgrade_unknown_hello_refusal_is_terminal() {
    reconnect_without_ack(Some("future_terminal_refusal")).await;
}

#[tokio::test]
async fn live_upgrade_deadline_abandons_an_unanswered_hello() {
    reconnect_without_ack(None).await;
}
