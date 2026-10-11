use super::*;
use crate::{
    forwarding::{ClientRouteKey, RouteBindRelayOutcome},
    router::{FrameSink, OutboundFrame},
    ConnectionId, ControlHandler, ForwardingTable, Registry, SupervisorHandle,
};
use std::sync::Condvar;
use subc_protocol::{
    manifest::{Concurrency, ExecutionMode, IdentityScope, ModuleManifest, ProviderRole, Tool},
    ModuleHelloAckBody, ModuleHelloBody, PROTOCOL_VERSION,
};
use tokio::sync::mpsc;

fn frame(ty: FrameType, corr: u64, body: serde_json::Value) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}
fn body(frame: &Frame) -> serde_json::Value {
    serde_json::from_slice(&frame.body).unwrap()
}
fn assert_reason(frame: &Frame, reason: &str) {
    assert_eq!(body(frame)["detail"]["reason"], reason);
}
fn ctx(id: u64, capacity: usize) -> (RouteCtx, mpsc::Receiver<OutboundFrame>) {
    let (tx, rx) = mpsc::channel(capacity);
    (
        RouteCtx {
            connection_id: ConnectionId::new(id),
            egress: FrameSink::new(tx),
        },
        rx,
    )
}
async fn receive(rx: &mut mpsc::Receiver<OutboundFrame>) -> Frame {
    tokio::time::timeout(Duration::from_secs(4), rx.recv())
        .await
        .expect("answer deadline")
        .expect("answer")
        .frame
}

struct Fixture {
    forwarding: Arc<ForwardingTable>,
    handler: Arc<ControlHandler>,
    supervisor: SupervisorHandle,
}
struct Module {
    ctx: RouteCtx,
    rx: mpsc::Receiver<OutboundFrame>,
    key: ModuleRouteKey,
    epoch: u32,
    client: ClientRouteKey,
    client_epoch: u32,
    _client_rx: mpsc::Receiver<OutboundFrame>,
}
impl Fixture {
    fn new(provider: Arc<dyn OperatorProvider>, limits: OperatorLimits) -> Self {
        let forwarding = Arc::new(ForwardingTable::default());
        forwarding.operator_confirms().configure(provider, limits);
        let supervisor = SupervisorHandle::new();
        let handler = Arc::new(
            ControlHandler::with_forwarding(Arc::new(Registry::default()), forwarding.clone())
                .with_supervisor(supervisor.clone()),
        );
        Self {
            forwarding,
            handler,
            supervisor,
        }
    }
    async fn module(&self, name: &str, id: u64, nonce: &str) -> Module {
        let (ctx, mut rx) = ctx(id, 8);
        self.supervisor.set_spawn_nonce(name, nonce.into());
        let manifest = ModuleManifest::builder(name, "0.1.0")
            .protocol_ver(PROTOCOL_VERSION)
            .provides(vec![ProviderRole::ToolProvider {
                tools: vec![Tool {
                    name: "read".into(),
                    description: None,
                    execution_mode: ExecutionMode::Pure,
                    schema: serde_json::json!({}),
                }],
                identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
                concurrency: Concurrency::ModuleManaged,
                emits_push: true,
                sub_supervises: false,
            }])
            .build();
        let hello = ModuleHelloBody {
            manifest,
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: Some(nonce.into()),
        };
        assert!(self
            .handler
            .handle_control_frame(
                &ctx,
                frame(FrameType::Hello, 1, serde_json::to_value(hello).unwrap())
            )
            .await
            .unwrap()
            .is_empty());
        let ack = receive(&mut rx).await;
        let ack: ModuleHelloAckBody = serde_json::from_slice(&ack.body).unwrap();
        assert!(ack.subc_ops.contains(&"operator.confirm".into()));
        self.forwarding.cutover_candidate(name).unwrap();
        let (client, mut client_rx) = ctx_fn(id + 10000);
        let pending = self
            .forwarding
            .begin_route_bind_relay_for_test(client.connection_id, client.egress.clone(), 2, name)
            .unwrap();
        self.forwarding
            .complete_pending_relay(
                ctx.connection_id,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        let opened = body(&receive(&mut client_rx).await);
        Module {
            key: ModuleRouteKey {
                endpoint: pending.endpoint,
                channel: pending.module_channel,
            },
            epoch: pending.module_epoch,
            ctx,
            rx,
            client: ClientRouteKey {
                connection_id: client.connection_id,
                channel: opened["route_channel"].as_u64().unwrap() as u16,
            },
            client_epoch: opened["route_epoch"].as_u64().unwrap() as u32,
            _client_rx: client_rx,
        }
    }
    async fn request(&self, module: &Module, corr: u64, summary: &str) -> Vec<Frame> {
        self.handler
            .handle_control_frame(
                &module.ctx,
                frame(
                    FrameType::Request,
                    corr,
                    serde_json::to_value(OperatorConfirmRequest::new(
                        summary,
                        module.key.channel,
                        module.epoch,
                    ))
                    .unwrap(),
                ),
            )
            .await
            .unwrap()
    }
    fn close(&self, module: &Module) {
        self.forwarding
            .release_client_route(
                module.client.connection_id,
                module.client.channel,
                module.client_epoch,
            )
            .unwrap();
    }
}
fn ctx_fn(id: u64) -> (RouteCtx, mpsc::Receiver<OutboundFrame>) {
    ctx(id, 8)
}

struct Immediate(ProviderResult);
impl OperatorProvider for Immediate {
    fn prompt(
        &self,
        _: &str,
        _: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
    ) -> ProviderResult {
        self.0
    }
}
#[derive(Debug, PartialEq)]
enum Event {
    Shown(String),
    Withdraw(String),
    Returned,
}
struct Controlled {
    events: mpsc::UnboundedSender<Event>,
    gate: Mutex<bool>,
    wake: Condvar,
    result: ProviderResult,
    delayed_handle: bool,
    ignores_withdraw: bool,
    withdraw_delay: Duration,
}
impl Controlled {
    fn new(
        result: ProviderResult,
        delayed_handle: bool,
        ignores_withdraw: bool,
        withdraw_delay: Duration,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Event>) {
        let (events, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                events,
                gate: Mutex::new(false),
                wake: Condvar::new(),
                result,
                delayed_handle,
                ignores_withdraw,
                withdraw_delay,
            }),
            rx,
        )
    }
    fn release(&self) {
        *self.gate.lock().unwrap() = true;
        self.wake.notify_all();
    }
    fn wait(&self) {
        let gate = self.gate.lock().unwrap();
        let _ = self
            .wake
            .wait_timeout_while(gate, Duration::from_secs(5), |open| !*open)
            .unwrap();
    }
}
struct ControlHandle(Arc<Controlled>);
impl OperatorWithdraw for ControlHandle {
    fn withdraw(&self, reason: &str) {
        self.0.events.send(Event::Withdraw(reason.into())).ok();
        std::thread::sleep(self.0.withdraw_delay);
        if !self.0.ignores_withdraw {
            self.0.release();
        }
    }
}
impl OperatorProvider for Arc<Controlled> {
    fn prompt(
        &self,
        text: &str,
        publish: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
    ) -> ProviderResult {
        if self.delayed_handle {
            self.wait();
        }
        self.events.send(Event::Shown(text.into())).ok();
        publish(Arc::new(ControlHandle(self.clone())));
        self.wait();
        self.events.send(Event::Returned).ok();
        self.result
    }
}
async fn event(rx: &mut mpsc::UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(4), rx.recv())
        .await
        .expect("provider event deadline")
        .expect("event")
}
fn short_limits() -> OperatorLimits {
    OperatorLimits {
        queue_wait: Duration::from_secs(2),
        prompt_timeout: Duration::from_millis(200),
        stuck_grace: Duration::from_millis(100),
    }
}

macro_rules! summary_refusal {
    ($name:ident, $summary:expr) => {
        #[test]
        fn $name() {
            let summary: String = $summary.into();
            assert!(!valid_summary(&summary));
        }
    };
}
summary_refusal!(summary_empty, "");
summary_refusal!(summary_201_scalars, "é".repeat(201));
summary_refusal!(summary_cr, "x\rx");
summary_refusal!(summary_lf, "x\nx");
summary_refusal!(summary_tab, "x\tx");
summary_refusal!(summary_line_separator, "x\u{2028}x");
summary_refusal!(summary_bidi_override, "x\u{202e}x");
summary_refusal!(summary_zero_width_space, "x\u{200b}x");
summary_refusal!(summary_paragraph_separator, "x\u{2029}x");
summary_refusal!(summary_word_joiner, "x\u{2060}x");
summary_refusal!(summary_bom, "x\u{feff}x");
summary_refusal!(summary_leading_whitespace, " x");
summary_refusal!(summary_trailing_whitespace, "x\u{2003}");
#[test]
fn summary_200_multibyte_and_combining_scalars() {
    assert!(valid_summary(&"é".repeat(200)));
    assert!(valid_summary("e\u{301}"));
}

#[tokio::test]
async fn undecodable_confirm_and_unregistered_check_precede_permission() {
    let f = Fixture::new(
        Arc::new(Immediate(ProviderResult::Approved)),
        short_limits(),
    );
    let m = f.module("m", 1, "nonce").await;
    let malformed = f
        .handler
        .handle_control_frame(
            &m.ctx,
            frame(
                FrameType::Request,
                4,
                serde_json::json!({"op":"operator.confirm"}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(body(&malformed[0])["code"], "invalid_control_body");
    let (raw, _rx) = ctx_fn(70);
    let refused = f
        .handler
        .handle_control_frame(
            &raw,
            frame(
                FrameType::Request,
                4,
                serde_json::json!({"op":"operator.confirm"}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(body(&refused[0])["code"], "not_registered");
}
#[tokio::test]
async fn unverified_binding_and_missing_nonce_never_prompt() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
    let f = Fixture::new(Arc::new(provider), short_limits());
    let m = f.module("m", 1, "nonce").await;
    f.forwarding
        .inject_operator_principal(m.key, Principal::Unverified);
    assert_eq!(
        body(&f.request(&m, 4, "valid").await[0])["code"],
        error_codes::OPERATOR_REQUEST_NOT_PERMITTED
    );
    f.forwarding
        .inject_operator_principal(m.key, Principal::Direct);
    f.supervisor.set_spawn_nonce("m", "changed".into());
    assert_eq!(
        body(&f.request(&m, 5, "valid").await[0])["code"],
        error_codes::OPERATOR_REQUEST_NOT_PERMITTED
    );
    assert!(events.try_recv().is_err());
}
#[tokio::test]
async fn swap_outgoing_and_incoming_presented_nonces_are_proven() {
    let f = Fixture::new(
        Arc::new(Immediate(ProviderResult::Approved)),
        short_limits(),
    );
    let mut outgoing = f.module("swap", 1, "old").await;
    f.supervisor.open_swap("swap", "new".into());
    f.supervisor.set_spawn_nonce("swap", "new".into());
    assert!(f.request(&outgoing, 4, "valid").await.is_empty());
    assert_eq!(
        body(&receive(&mut outgoing.rx).await)["outcome"],
        "confirmed"
    );
    // The incoming endpoint is promoted while the outgoing route remains live.
    let mut incoming = f.module("swap", 2, "new").await;
    assert!(f.request(&incoming, 5, "valid").await.is_empty());
    assert_eq!(
        body(&receive(&mut incoming.rx).await)["outcome"],
        "confirmed"
    );
    assert!(f.request(&outgoing, 6, "valid").await.is_empty());
    assert_eq!(
        body(&receive(&mut outgoing.rx).await)["outcome"],
        "confirmed"
    );
}
#[tokio::test]
async fn provider_error_is_not_confirmation() {
    let f = Fixture::new(
        Arc::new(Immediate(ProviderResult::Unavailable)),
        short_limits(),
    );
    let mut m = f.module("m", 1, "nonce").await;
    assert!(f.request(&m, 4, "valid").await.is_empty());
    let answer = receive(&mut m.rx).await;
    assert_eq!(
        body(&answer)["code"],
        error_codes::OPERATOR_PRESENCE_UNAVAILABLE
    );
    assert_reason(&answer, "provider_error");
}
#[tokio::test]
async fn route_close_withdraws_and_discards_late_provider_error() {
    let (dispatch, messages) = log_capture();
    let _guard = tracing::dispatcher::set_default(&dispatch);
    let (provider, mut events) =
        Controlled::new(ProviderResult::Unavailable, false, false, Duration::ZERO);
    let f = Fixture::new(Arc::new(provider), short_limits());
    let mut m = f.module("m", 1, "nonce").await;
    assert!(f.request(&m, 4, "valid").await.is_empty());
    assert_eq!(
        event(&mut events).await,
        Event::Shown("m asks: valid (requested by a local program)".into())
    );
    f.close(&m);
    let answer = receive(&mut m.rx).await;
    assert_eq!(body(&answer)["code"], error_codes::OPERATOR_DECLINED);
    assert_reason(&answer, "route_closed");
    assert_eq!(
        event(&mut events).await,
        Event::Withdraw("route_closed".into())
    );
    assert_eq!(event(&mut events).await, Event::Returned);
    tokio::time::timeout(Duration::from_secs(1), async {
        while f.forwarding.operator_confirms().lock().prompt.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        m.rx.try_recv().is_err(),
        "late provider errors must not send a second answer"
    );
    assert_eq!(
        messages
            .lock()
            .unwrap()
            .iter()
            .filter(|message| *message == "operator_confirm_audit")
            .count(),
        1
    );
    let cancel = f
        .handler
        .handle_control_frame(&m.ctx, frame(FrameType::Cancel, 4, serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(body(&cancel[0])["code"], "unknown_subscription");
}
#[tokio::test]
async fn module_disconnect_commits_before_route_release() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Unavailable, false, false, Duration::ZERO);
    let f = Fixture::new(Arc::new(provider), short_limits());
    let mut m = f.module("m", 1, "nonce").await;
    assert!(f.request(&m, 4, "valid").await.is_empty());
    event(&mut events).await;
    f.handler.cleanup_connection(m.ctx.connection_id).unwrap();
    assert_reason(&receive(&mut m.rx).await, "module_closed");
    assert_eq!(
        event(&mut events).await,
        Event::Withdraw("module_closed".into())
    );
    event(&mut events).await;
}
#[tokio::test]
async fn cancel_withdraws_only_asking_connection_and_corr() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
    let f = Fixture::new(Arc::new(provider), short_limits());
    let mut m = f.module("m", 1, "nonce").await;
    assert!(f.request(&m, 4, "valid").await.is_empty());
    event(&mut events).await;
    let (other, _rx) = ctx_fn(999);
    let wrong = f
        .handler
        .handle_control_frame(&other, frame(FrameType::Cancel, 4, serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(body(&wrong[0])["code"], "unknown_subscription");
    assert!(f
        .handler
        .handle_control_frame(&m.ctx, frame(FrameType::Cancel, 4, serde_json::json!({})))
        .await
        .unwrap()
        .is_empty());
    assert_reason(&receive(&mut m.rx).await, "caller_cancelled");
    assert_eq!(
        event(&mut events).await,
        Event::Withdraw("caller_cancelled".into())
    );
    event(&mut events).await;
}
#[tokio::test]
async fn fifo_module_limit_and_queue_full() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, true, Duration::ZERO);
    let f = Fixture::new(Arc::new(provider.clone()), OperatorLimits::default());
    let mut modules = Vec::new();
    for i in 1..=6 {
        modules.push(f.module(&format!("m{i}"), i, "nonce").await);
    }
    assert!(f.request(&modules[0], 4, "first").await.is_empty());
    event(&mut events).await;
    assert_reason(&f.request(&modules[0], 5, "again").await[0], "module_limit");
    for m in &modules[1..5] {
        assert!(f.request(m, 4, "waiting").await.is_empty());
    }
    assert_reason(&f.request(&modules[5], 4, "full").await[0], "queue_full");
    provider.release();
    for (index, m) in modules[..5].iter_mut().enumerate() {
        if index > 0 {
            assert_eq!(
                event(&mut events).await,
                Event::Shown(format!(
                    "m{} asks: waiting (requested by a local program)",
                    index + 1
                ))
            );
        }
        assert_eq!(event(&mut events).await, Event::Returned);
        assert_eq!(body(&receive(&mut m.rx).await)["outcome"], "confirmed");
    }
}
#[tokio::test]
async fn timeout_stuck_and_prompt_slot_wait_for_provider_return() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, true, Duration::ZERO);
    let f = Fixture::new(Arc::new(provider.clone()), short_limits());
    let mut first = f.module("first", 1, "nonce").await;
    let mut next = f.module("next", 2, "nonce").await;
    assert!(f.request(&first, 4, "first").await.is_empty());
    event(&mut events).await;
    assert!(f.request(&next, 4, "next").await.is_empty());
    assert_reason(&receive(&mut first.rx).await, "timeout");
    assert_eq!(event(&mut events).await, Event::Withdraw("timeout".into()));
    assert_reason(&receive(&mut next.rx).await, "provider_stuck");
    assert_reason(&f.request(&next, 5, "next").await[0], "provider_stuck");
    assert!(
        events.try_recv().is_err(),
        "no second prompt while the call is running"
    );
    provider.release();
    assert_eq!(event(&mut events).await, Event::Returned);
    // Synchronize on the state transition, not on a sleep or provider's event.
    tokio::time::timeout(Duration::from_secs(1), async {
        while f.forwarding.operator_confirms().lock().stuck {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(f.request(&next, 6, "next").await.is_empty());
    assert!(matches!(event(&mut events).await, Event::Shown(_)));
    event(&mut events).await;
    assert_eq!(body(&receive(&mut next.rx).await)["outcome"], "confirmed");
}
#[tokio::test]
async fn queue_wait_timer_is_independent_of_hung_prompt() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, true, Duration::ZERO);
    let limits = OperatorLimits {
        queue_wait: Duration::from_millis(40),
        prompt_timeout: Duration::from_secs(2),
        stuck_grace: Duration::from_millis(100),
    };
    let f = Fixture::new(Arc::new(provider.clone()), limits);
    let mut first = f.module("first", 1, "nonce").await;
    let mut queued = f.module("queued", 2, "nonce").await;
    f.request(&first, 4, "first").await;
    event(&mut events).await;
    f.request(&queued, 4, "queued").await;
    assert_reason(&receive(&mut queued.rx).await, "queue_wait");
    assert!(events.try_recv().is_err());
    provider.release();
    event(&mut events).await;
    receive(&mut first.rx).await;
}
#[tokio::test]
async fn stuck_precedes_module_limit_with_undelivered_answer() {
    let f = Fixture::new(
        Arc::new(Immediate(ProviderResult::Approved)),
        short_limits(),
    );
    let m = f.module("m", 1, "nonce").await;
    let confirms = f.forwarding.operator_confirms();
    for corr in 10..18 {
        m.ctx
            .egress
            .try_send(frame(FrameType::Pong, corr, serde_json::json!({})))
            .unwrap();
    }
    assert!(f.request(&m, 4, "valid").await.is_empty());
    tokio::time::timeout(Duration::from_secs(1), async {
        while {
            let state = confirms.lock();
            state.prompt.is_some() || state.deliveries != 1
        } {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the actual answer is blocked on full egress");
    assert!(confirms.lock().modules.contains_key("m"));
    confirms.lock().stuck = true;
    assert_reason(&f.request(&m, 5, "valid").await[0], "provider_stuck");
}
#[tokio::test]
async fn backoff_uses_module_and_principal_not_route() {
    let f = Fixture::new(
        Arc::new(Immediate(ProviderResult::Declined)),
        short_limits(),
    );
    let mut first = f.module("m", 1, "nonce").await;
    f.request(&first, 4, "valid").await;
    assert_reason(&receive(&mut first.rx).await, "person");
    f.handler
        .cleanup_connection(first.ctx.connection_id)
        .unwrap();
    let new_route = f.module("m", 2, "nonce").await;
    assert_reason(&f.request(&new_route, 5, "valid").await[0], "backoff");
}
#[tokio::test]
async fn delayed_withdraw_handle_does_not_delay_timeout_or_routing() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, true, true, Duration::ZERO);
    let f = Fixture::new(Arc::new(provider.clone()), short_limits());
    let mut m = f.module("m", 1, "nonce").await;
    assert!(f.request(&m, 4, "valid").await.is_empty());
    assert_reason(&receive(&mut m.rx).await, "timeout");
    assert!(f.forwarding.operator_confirms().lock().prompt.is_some());
    let pong = f
        .handler
        .handle_control_frame(&m.ctx, frame(FrameType::Ping, 10, serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(pong[0].header.ty, FrameType::Pong);
    let mut unrelated = f.module("other", 2, "nonce").await;
    tokio::time::timeout(
        Duration::from_millis(500),
        open_route_while_provider_runs(&f, &mut unrelated, "other"),
    )
    .await
    .expect("route.open must not wait for prompt setup");
    provider.release();
    assert!(matches!(event(&mut events).await, Event::Shown(_)));
    let mut tail = vec![event(&mut events).await, event(&mut events).await];
    assert!(tail.contains(&Event::Withdraw("timeout".into())));
    assert!(tail.contains(&Event::Returned));
    tail.clear();
}
#[tokio::test]
async fn blocking_withdraw_does_not_delay_route_open_or_stuck_grace() {
    let (provider, mut events) = Controlled::new(
        ProviderResult::Unavailable,
        false,
        false,
        Duration::from_secs(2),
    );
    let f = Fixture::new(Arc::new(provider.clone()), short_limits());
    let mut m = f.module("m", 1, "nonce").await;
    f.request(&m, 4, "valid").await;
    event(&mut events).await;
    assert_reason(&receive(&mut m.rx).await, "timeout");
    assert_eq!(event(&mut events).await, Event::Withdraw("timeout".into()));
    let mut other = f.module("other", 2, "nonce").await;
    tokio::time::timeout(
        Duration::from_millis(500),
        open_route_while_provider_runs(&f, &mut other, "other"),
    )
    .await
    .expect("route.open must not wait for withdraw");
    tokio::time::timeout(Duration::from_millis(500), async {
        while !f.forwarding.operator_confirms().lock().stuck {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stuck grace runs while withdraw is blocked");
    assert!(events.try_recv().is_err());
    assert_eq!(event(&mut events).await, Event::Returned);
}
#[tokio::test]
async fn saturated_egress_bounds_deliveries_and_does_not_block_other_prompts() {
    let (dispatch, messages) = log_capture();
    let _guard = tracing::dispatcher::set_default(&dispatch);
    let f = Fixture::new(
        Arc::new(Immediate(ProviderResult::Approved)),
        OperatorLimits {
            queue_wait: Duration::ZERO,
            prompt_timeout: Duration::ZERO,
            stuck_grace: Duration::ZERO,
        },
    );
    let unread = f.module("unread", 1, "nonce").await;
    let mut other = f.module("other", 2, "nonce").await;
    for corr in 10..18 {
        unread
            .ctx
            .egress
            .try_send(frame(FrameType::Pong, corr, serde_json::json!({})))
            .unwrap();
    }
    // Nonzero prompt time avoids timing out a fast blocking call under load.
    f.forwarding
        .operator_confirms()
        .lock()
        .limits
        .prompt_timeout = Duration::from_secs(1);
    assert!(f.request(&unread, 4, "valid").await.is_empty());
    tokio::time::timeout(Duration::from_secs(1), async {
        while f.forwarding.operator_confirms().lock().prompt.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for corr in 100..1100 {
        assert_reason(&f.request(&unread, corr, "valid").await[0], "module_limit");
    }
    assert_eq!(f.forwarding.operator_confirms().lock().modules.len(), 1);
    assert_eq!(f.forwarding.operator_confirms().lock().deliveries, 1);
    assert!(f.request(&other, 4, "valid").await.is_empty());
    assert_eq!(body(&receive(&mut other.rx).await)["outcome"], "confirmed");
    // Advance the absolute delivery deadline without waiting eleven real seconds.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(12)).await;
    tokio::task::yield_now().await;
    assert!(f.forwarding.operator_confirms().lock().modules.is_empty());
    assert_eq!(f.forwarding.operator_confirms().lock().deliveries, 0);
    assert!(messages
        .lock()
        .unwrap()
        .iter()
        .any(|message| message == "operator confirm answer dropped"));
}

#[test]
fn refused_audit_has_exact_fields_and_debug_escaped_summary() {
    use tracing::{
        field::{Field, Visit},
        Event, Subscriber,
    };
    use tracing_subscriber::{layer::Context, prelude::*, Layer};
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<HashMap<String, String>>>>);
    struct Fields(HashMap<String, String>);
    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            // Preserve string values as the formatter does, rather than
            // adding Debug escaping in the collector and hiding a missing `?`.
            self.0.insert(field.name().into(), value.to_owned());
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().into(), format!("{value:?}"));
        }
    }
    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            if event.metadata().target() == "subc_daemon::operator_confirm" {
                let mut fields = Fields(HashMap::new());
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields.0);
            }
        }
    }
    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(captured.clone()));
    tracing::subscriber::with_default(subscriber, || {
        for summary in ["", "x\r", "x\n", "x\t", "x\u{202e}"] {
            audit(
                "m",
                summary,
                "direct",
                Outcome::refusal(error_codes::OPERATOR_SUMMARY_INVALID),
                Duration::ZERO,
                Duration::ZERO,
                false,
            );
        }
    });
    for fields in captured.lock().unwrap().iter() {
        let mut keys: Vec<_> = fields.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "message",
                "module_id",
                "outcome",
                "principal",
                "prompt_ms",
                "prompt_shown",
                "reason",
                "summary",
                "wait_ms"
            ]
        );
        assert_eq!(fields["message"], "operator_confirm_audit");
        assert!(!fields["summary"].contains(['\n', '\r', '\t', '\u{202e}']));
        assert!(fields["summary"].starts_with('"'));
    }
    assert_eq!(captured.lock().unwrap().len(), 5);
}

#[cfg(feature = "test-support")]
#[test]
fn script_provider_is_ckdev_only_and_records_events() {
    let dir = cortexkit_test_support::ScratchDir::new("operator-script");
    let script = dir.join("script");
    let events = dir.join("events");
    std::fs::write(
        &script,
        "approve\ndecline\nunavailable\nhang\napprove when ".to_owned()
            + dir.join("ready").to_str().unwrap(),
    )
    .unwrap();
    let mut config = crate::bootstrap::BootstrapConfig::new(dir.join("connection"), 0);
    config.operator_script = Some(script);
    config.operator_events = Some(events.clone());
    let refused = super::test_provider::select(
        Arc::new(Immediate(ProviderResult::NoPresence)),
        &config,
        Some(std::path::Path::new("ck-subc")),
    );
    assert_eq!(
        refused.prompt("text", Box::new(|_| {})),
        ProviderResult::NoPresence
    );
    assert!(!events.exists());
    let selected = super::test_provider::select(
        Arc::new(Immediate(ProviderResult::NoPresence)),
        &config,
        Some(std::path::Path::new("ckdev-subc")),
    );
    assert_eq!(
        selected.prompt("text", Box::new(|_| {})),
        ProviderResult::Approved
    );
    assert_eq!(
        selected.prompt("text", Box::new(|_| {})),
        ProviderResult::Declined
    );
    assert_eq!(
        selected.prompt("text", Box::new(|_| {})),
        ProviderResult::Unavailable
    );
    assert_eq!(
        selected.prompt("text", Box::new(|handle| handle.withdraw("route_closed"))),
        ProviderResult::Unavailable
    );
    std::fs::write(dir.join("ready"), "").unwrap();
    assert_eq!(
        selected.prompt("text", Box::new(|_| {})),
        ProviderResult::Approved
    );
    assert_eq!(
        selected.prompt("text", Box::new(|_| {})),
        ProviderResult::Unavailable
    );
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(events)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 13);
    assert_eq!(
        lines[0],
        serde_json::json!({"event":"prompt_shown", "text":"text"})
    );
    assert_eq!(
        lines[7],
        serde_json::json!({"event":"withdraw", "reason":"route_closed"})
    );
    assert_eq!(
        lines[8],
        serde_json::json!({"event":"provider_returned", "result":"error"})
    );
    assert_eq!(
        lines[12],
        serde_json::json!({"event":"provider_returned", "result":"unavailable"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_between_binding_lookup_and_admission_is_never_lost() {
    let (provider, _events) =
        Controlled::new(ProviderResult::Approved, false, true, Duration::ZERO);
    let f = Fixture::new(Arc::new(provider.clone()), short_limits());
    let mut m = f.module("m", 1, "nonce").await;
    let lookup = Arc::new(std::sync::Barrier::new(2));
    let forwarding = f.forwarding.clone();
    let confirms = forwarding.operator_confirms();
    let request_ctx = m.ctx.clone();
    let key = m.key;
    let epoch = m.epoch;
    let runtime = tokio::runtime::Handle::current();
    let barrier = lookup.clone();
    let admission = std::thread::spawn(move || {
        let _entered = runtime.enter();
        forwarding
            .with_operator_route(key.endpoint.connection_id, key.channel, epoch, |binding| {
                // The route close running on the other thread is now waiting for
                // the forwarding table's write lock. This admission's lookup began
                // before that close, and the close must still see it and withdraw it.
                barrier.wait();
                confirms.admit(
                    &request_ctx,
                    frame(FrameType::Request, 4, serde_json::json!({})),
                    "m".into(),
                    true,
                    OperatorConfirmRequest::new("valid", key.channel, epoch),
                    binding,
                )
            })
            .unwrap()
    });
    lookup.wait();
    f.close(&m);
    let immediate = admission.join().unwrap();
    if immediate.is_empty() {
        let answer = receive(&mut m.rx).await;
        assert_reason(&answer, "route_closed");
    } else {
        assert_eq!(
            body(&immediate[0])["code"],
            error_codes::OPERATOR_REQUEST_NOT_PERMITTED
        );
    }
    assert_eq!(
        body(&f.request(&m, 5, "valid").await[0])["code"],
        error_codes::OPERATOR_REQUEST_NOT_PERMITTED
    );
    provider.release();
}

#[tokio::test]
async fn adjacent_check_pairs_keep_the_first_refusal() {
    let f = Fixture::new(
        Arc::new(Immediate(ProviderResult::Approved)),
        short_limits(),
    );
    let m = f.module("m", 1, "nonce").await;
    f.supervisor.set_spawn_nonce("m", "wrong".into());
    assert_eq!(
        body(&f.request(&m, 4, "").await[0])["code"],
        error_codes::OPERATOR_REQUEST_NOT_PERMITTED
    );
    f.supervisor.set_spawn_nonce("m", "nonce".into());
    let confirms = f.forwarding.operator_confirms();
    confirms.lock().backoff.insert(
        ("m".into(), "direct".into()),
        Instant::now() + Duration::from_secs(30),
    );
    assert_eq!(
        body(&f.request(&m, 5, "").await[0])["code"],
        error_codes::OPERATOR_SUMMARY_INVALID
    );
    confirms.lock().stuck = true;
    assert_reason(&f.request(&m, 6, "valid").await[0], "backoff");
    {
        let mut state = confirms.lock();
        state.backoff.clear();
        state.stuck = false;
        state.modules.insert("m".into(), 999);
    }
    // With this module already holding its one slot, the per-module limit
    // refuses before the queue's capacity is consulted. A genuinely full queue
    // is exercised in fifo_module_limit_and_queue_full.
    assert_reason(&f.request(&m, 7, "valid").await[0], "module_limit");
}

async fn open_route_while_provider_runs(f: &Fixture, module: &mut Module, name: &str) {
    let dir = cortexkit_test_support::ScratchDir::new("operator-concurrent-route");
    let (client, mut rx) = ctx_fn(9000);
    let request = subc_control::ClientControlRequest::RouteOpen {
        target: subc_protocol::RouteTarget::ToolProvider {
            module_id: name.into(),
        },
        identity: subc_protocol::BindIdentity::new(dir.path().to_path_buf(), "test", "session"),
        consumer_identity: None,
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope: None,
    };
    let handler = f.handler.clone();
    let call = tokio::spawn(async move {
        handler
            .handle_control_frame(
                &client,
                frame(
                    FrameType::Request,
                    50,
                    serde_json::to_value(request).unwrap(),
                ),
            )
            .await
            .unwrap()
    });
    let bind = receive(&mut module.rx).await;
    assert_eq!(body(&bind)["op"], "route.bind");
    f.handler
        .handle_control_frame(
            &module.ctx,
            frame(
                FrameType::Response,
                bind.header.corr,
                serde_json::json!({"op":"route.bind"}),
            ),
        )
        .await
        .unwrap();
    let returned = call.await.unwrap();
    assert!(
        returned.is_empty(),
        "route.open response is published with its binding"
    );
    assert_eq!(body(&receive(&mut rx).await)["op"], "route.open");
}

fn log_capture() -> (tracing::Dispatch, Arc<Mutex<Vec<String>>>) {
    use tracing::{
        field::{Field, Visit},
        Event, Subscriber,
    };
    use tracing_subscriber::{layer::Context, prelude::*, Layer};
    struct Messages(Arc<Mutex<Vec<String>>>);
    struct Message(Option<String>);
    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }
    impl<S: Subscriber> Layer<S> for Messages {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            if event.metadata().target() == "subc_daemon::operator_confirm" {
                let mut message = Message(None);
                event.record(&mut message);
                if let Some(message) = message.0 {
                    self.0.lock().unwrap().push(message);
                }
            }
        }
    }
    let messages = Arc::new(Mutex::new(Vec::new()));
    (
        tracing::Dispatch::new(tracing_subscriber::registry().with(Messages(messages.clone()))),
        messages,
    )
}

fn privileged_fixture(provider: Arc<dyn OperatorProvider>, limits: OperatorLimits) -> Fixture {
    let mut f = Fixture::new(provider, limits);
    f.handler =
        Arc::new((*f.handler).clone().with_privileged_operator_prompt(
            crate::daemon_config::PrivilegedOperatorPrompt::Enforce,
        ));
    f
}

fn restart(corr: u64) -> Frame {
    frame(
        FrameType::Request,
        corr,
        serde_json::json!({
            "op":"supervisor.restart", "module_id":"held-target", "drain_timeout_ms":137
        }),
    )
}

fn held_restart(f: &Fixture, client: RouteCtx, corr: u64) -> tokio::task::JoinHandle<Vec<Frame>> {
    let handler = f.handler.clone();
    tokio::spawn(async move {
        handler
            .handle_control_frame(&client, restart(corr))
            .await
            .unwrap()
    })
}

async fn decision(task: tokio::task::JoinHandle<Vec<Frame>>) -> Frame {
    tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .expect("privileged request terminates")
        .unwrap()
        .remove(0)
}

#[tokio::test]
async fn privileged_restart_approval_holds_dispatch_until_provider_approves() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
    let f = privileged_fixture(Arc::new(provider.clone()), OperatorLimits::default());
    let (client, _rx) = ctx_fn(800);
    let task = held_restart(&f, client, 137);
    let Event::Shown(text) = event(&mut events).await else {
        panic!("prompt must be shown")
    };
    assert_eq!(
        text,
        "Restart module held-target — requested by an unidentified local process"
    );
    assert!(
        !task.is_finished(),
        "restart must not dispatch before approval"
    );
    provider.release();
    let answer = decision(task).await;
    assert_eq!(answer.header.corr, 137);
    assert_eq!(
        body(&answer)["code"],
        "unknown_module",
        "approved request reaches the real restart handler"
    );
}

#[tokio::test]
async fn privileged_restart_decline_never_dispatches() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Declined, false, false, Duration::ZERO);
    let f = privileged_fixture(Arc::new(provider.clone()), OperatorLimits::default());
    let (client, _rx) = ctx_fn(801);
    let task = held_restart(&f, client, 138);
    assert!(matches!(event(&mut events).await, Event::Shown(_)));
    assert!(!task.is_finished());
    provider.release();
    let answer = decision(task).await;
    assert_eq!(body(&answer)["code"], error_codes::OPERATOR_DECLINED);
    assert_reason(&answer, "person");
}

#[tokio::test]
async fn privileged_restart_unavailable_never_dispatches() {
    let f = privileged_fixture(
        Arc::new(Immediate(ProviderResult::Unavailable)),
        OperatorLimits::default(),
    );
    let (client, _rx) = ctx_fn(802);
    let answer = decision(held_restart(&f, client, 139)).await;
    assert_eq!(
        body(&answer)["code"],
        error_codes::OPERATOR_PRESENCE_UNAVAILABLE
    );
    assert_reason(&answer, "provider_error");
}

#[tokio::test]
async fn privileged_registered_module_restart_is_unsupported_in_both_modes() {
    for mode in [
        crate::daemon_config::PrivilegedOperatorPrompt::Log,
        crate::daemon_config::PrivilegedOperatorPrompt::Enforce,
    ] {
        let (provider, mut events) =
            Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
        let mut f = privileged_fixture(Arc::new(provider), OperatorLimits::default());
        f.handler = Arc::new(
            (*f.handler)
                .clone()
                .with_privileged_operator_prompt(mode)
                .with_caller_lookup_for_test(|_| {
                    panic!("a registered module must never trigger caller lookup")
                }),
        );
        let m = f.module("module-caller", 803, "nonce").await;
        #[cfg(unix)]
        let target = supervised_privileged_target(&f);
        #[cfg(unix)]
        let before = target.status().unwrap();
        let frames = f
            .handler
            .handle_control_frame(&m.ctx, restart(140))
            .await
            .unwrap();
        assert_eq!(body(&frames[0])["code"], "unsupported_control_frame");
        assert!(events.try_recv().is_err());
        #[cfg(unix)]
        {
            assert_eq!(
                target.status().unwrap().spawn_generation,
                before.spawn_generation
            );
            assert_eq!(target.status().unwrap().pid, before.pid);
            target.stop().await.unwrap();
        }
    }
}

#[tokio::test]
async fn privileged_reads_route_open_health_probe_and_preview_never_prompt_or_lookup() {
    let f = privileged_fixture(
        Arc::new(Immediate(ProviderResult::Approved)),
        OperatorLimits::default(),
    );
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let handler = (*f.handler).clone().with_caller_lookup_for_test(move |_| {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::caller_identity::CallerIdentity::default()
    });
    let (client, _rx) = ctx_fn(804);
    for request in [
        serde_json::json!({"op":"server.describe"}),
        serde_json::json!({"op":"catalog.list"}),
        serde_json::json!({"op":"supervisor.list"}),
        serde_json::json!({"op":"supervisor.health"}),
        serde_json::json!({"op":"supervisor.routes"}),
        serde_json::json!({"op":"supervisor.provenance"}),
        serde_json::json!({"op":"supervisor.stderr_tail","module_id":"missing"}),
        serde_json::json!({"op":"supervisor.terminals","module_id":"missing"}),
        serde_json::json!({"op":"supervisor.spawn_snapshot"}),
        serde_json::json!({"op":"supervisor.spawn_subscribe"}),
        serde_json::json!({"op":"supervisor.health_probe","module_id":"missing"}),
        serde_json::json!({"op":"supervisor.rescan","preview":true}),
        serde_json::json!({"op":"route.poll","route_channel":1,"route_epoch":1,"kind":"status"}),
        serde_json::json!({"op":"route.open","target":{"kind":"tool_provider","module_id":"missing"},"identity":{"project_root":"/test","harness":"test","session":"test"}}),
    ] {
        // Decode first so a malformed fixture cannot masquerade as a no-prompt read.
        let _: subc_control::ClientControlRequest =
            serde_json::from_value(request.clone()).unwrap();
        let replies = handler
            .handle_control_frame(&client, frame(FrameType::Request, 141, request))
            .await
            .unwrap();
        assert!(replies
            .iter()
            .all(|reply| body(reply)["code"] != error_codes::OPERATOR_PRESENCE_UNAVAILABLE));
        assert!(f.forwarding.operator_confirms().lock().prompt.is_none());
        assert!(f.forwarding.operator_confirms().lock().queue.is_empty());
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn privileged_caller_lookup_runs_on_a_blocking_thread() {
    let runtime_thread = std::thread::current().id();
    let f = privileged_fixture(
        Arc::new(Immediate(ProviderResult::Unavailable)),
        OperatorLimits::default(),
    );
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let handler = (*f.handler).clone().with_caller_lookup_for_test(move |_| {
        assert_ne!(std::thread::current().id(), runtime_thread);
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::caller_identity::CallerIdentity::default()
    });
    let (client, _rx) = ctx_fn(805);
    let replies = handler
        .handle_control_frame(&client, restart(142))
        .await
        .unwrap();
    assert_eq!(
        body(&replies[0])["code"],
        error_codes::OPERATOR_PRESENCE_UNAVAILABLE
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn privileged_and_module_requests_share_queue_full_and_queue_wait() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, true, Duration::ZERO);
    let f = privileged_fixture(
        Arc::new(provider.clone()),
        OperatorLimits {
            queue_wait: Duration::from_millis(150),
            prompt_timeout: Duration::from_secs(4),
            stuck_grace: Duration::from_millis(50),
        },
    );
    let m = f.module("shared", 806, "nonce").await;
    assert!(f.request(&m, 1, "module first").await.is_empty());
    assert!(matches!(event(&mut events).await, Event::Shown(_)));
    let mut tasks = Vec::new();
    for id in 810..814 {
        let (client, _rx) = ctx_fn(id);
        tasks.push(held_restart(&f, client, id));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.forwarding.operator_confirms().lock().queue.len() != 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (client, _rx) = ctx_fn(814);
    let answer = decision(held_restart(&f, client, 814)).await;
    assert_reason(&answer, "queue_full");
    for task in tasks {
        assert_reason(&decision(task).await, "queue_wait");
    }
    provider.release();
    receive(&mut { m.rx }).await;
}

#[tokio::test]
async fn privileged_requests_share_stuck_provider_refusal() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, true, Duration::ZERO);
    let f = privileged_fixture(Arc::new(provider.clone()), short_limits());
    let (client, _rx) = ctx_fn(820);
    let task = held_restart(&f, client, 143);
    assert!(matches!(event(&mut events).await, Event::Shown(_)));
    assert_reason(&decision(task).await, "timeout");
    assert_eq!(event(&mut events).await, Event::Withdraw("timeout".into()));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !f.forwarding.operator_confirms().lock().stuck {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (client, _rx) = ctx_fn(821);
    assert_reason(
        &decision(held_restart(&f, client, 144)).await,
        "provider_stuck",
    );
    provider.release();
}

#[test]
#[ignore = "child process fixture for privileged restart tests"]
fn privileged_child_waits_until_supervisor_teardown() {
    loop {
        std::thread::park();
    }
}

#[cfg(unix)]
fn supervised_privileged_target(f: &Fixture) -> crate::supervise::SupervisedModule {
    let registry = Arc::new(Registry::default());
    let supervisor = crate::Supervisor::new_for_test(registry, crate::RestartPolicy::default())
        .with_handle(f.supervisor.clone());
    let spec = crate::ModuleSpec {
        module_id: "held-target".into(),
        program: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".into(),
            "operator_confirm::tests::privileged_child_waits_until_supervisor_teardown".into(),
            "--ignored".into(),
        ],
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: subc_control::ModuleProtocol::None,
        overlap: crate::supervise::ModuleOverlap::Exclusive,
    };
    supervisor.supervise_configured(spec, true).unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn privileged_restart_approval_restarts_only_the_received_target() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
    let f = privileged_fixture(Arc::new(provider.clone()), OperatorLimits::default());
    let module = supervised_privileged_target(&f);
    let before = module.status().unwrap();
    let (client, _rx) = ctx_fn(830);
    let task = held_restart(&f, client, 145);
    assert!(matches!(event(&mut events).await, Event::Shown(_)));
    assert_eq!(
        module.status().unwrap().spawn_generation,
        before.spawn_generation
    );
    assert!(!task.is_finished());
    provider.release();
    let answer = decision(task).await;
    assert_eq!(answer.header.corr, 145);
    assert_eq!(
        body(&answer),
        serde_json::json!({"op":"supervisor.ack","module_id":"held-target","applied":true})
    );
    tokio::time::timeout(Duration::from_secs(4), async {
        while module.status().unwrap().spawn_generation == before.spawn_generation {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("approval actually replaces the child");
    assert_ne!(module.status().unwrap().pid, before.pid);
    module.stop().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn privileged_restart_decline_leaves_the_running_process_unchanged() {
    let f = privileged_fixture(
        Arc::new(Immediate(ProviderResult::Declined)),
        OperatorLimits::default(),
    );
    let module = supervised_privileged_target(&f);
    let before = module.status().unwrap();
    let (client, _rx) = ctx_fn(831);
    let answer = decision(held_restart(&f, client, 146)).await;
    assert_eq!(body(&answer)["code"], error_codes::OPERATOR_DECLINED);
    assert_reason(&answer, "person");
    assert_eq!(
        module.status().unwrap().spawn_generation,
        before.spawn_generation
    );
    assert_eq!(module.status().unwrap().pid, before.pid);
    module.stop().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn privileged_restart_unavailable_leaves_the_running_process_unchanged() {
    let f = privileged_fixture(
        Arc::new(Immediate(ProviderResult::Unavailable)),
        OperatorLimits::default(),
    );
    let module = supervised_privileged_target(&f);
    let before = module.status().unwrap();
    let (client, _rx) = ctx_fn(832);
    let answer = decision(held_restart(&f, client, 147)).await;
    assert_eq!(
        body(&answer)["code"],
        error_codes::OPERATOR_PRESENCE_UNAVAILABLE
    );
    assert_reason(&answer, "provider_error");
    assert_eq!(
        module.status().unwrap().spawn_generation,
        before.spawn_generation
    );
    assert_eq!(module.status().unwrap().pid, before.pid);
    module.stop().await.unwrap();
}

async fn privileged_socket(
    router: Arc<crate::Router>,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<Result<(), crate::server::ConnectionError>>,
) {
    use subc_transport::{authenticate_client, ConnectionInfo, Endpoint, SCHEMA_VERSION};
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let key = vec![0x42; 32];
    let daemon_id = [0x24; 16];
    let auth = crate::server::ServerAuth::new(key.clone(), daemon_id, "test-subc");
    let server = tokio::spawn(crate::server::handle_connection(server, router, auth));
    let connection = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: 1,
        }],
        key,
        daemon_id,
        pid: std::process::id(),
        daemon_ver: "test-subc".into(),
    };
    authenticate_client(&mut client, &connection, Duration::from_secs(2))
        .await
        .unwrap();
    (client, server)
}

async fn socket_reply(client: &mut tokio::io::DuplexStream) -> Frame {
    tokio::time::timeout(Duration::from_secs(2), crate::read_frame(client))
        .await
        .expect("reader must not wait behind an operator prompt")
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn privileged_prompt_does_not_hold_same_or_other_connection_reader() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
    let f = privileged_fixture(Arc::new(provider.clone()), OperatorLimits::default());
    let router = Arc::new(crate::Router::with_control_handler(f.handler.clone()));
    let (mut first, first_server) = privileged_socket(router.clone()).await;
    let (mut second, second_server) = privileged_socket(router).await;
    crate::write_frame(&mut first, &restart(150)).await.unwrap();
    assert!(matches!(event(&mut events).await, Event::Shown(_)));
    let read = frame(
        FrameType::Request,
        151,
        serde_json::json!({"op":"supervisor.list"}),
    );
    crate::write_frame(&mut first, &read).await.unwrap();
    crate::write_frame(&mut second, &read).await.unwrap();
    assert_eq!(socket_reply(&mut first).await.header.corr, 151);
    assert_eq!(socket_reply(&mut second).await.header.corr, 151);
    provider.release();
    assert_eq!(socket_reply(&mut first).await.header.corr, 150);
    drop(first);
    drop(second);
    first_server.await.unwrap().unwrap();
    second_server.await.unwrap().unwrap();
}

#[tokio::test]
async fn privileged_disconnect_withdraws_prompt_and_discards_late_approval() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
    let f = privileged_fixture(Arc::new(provider.clone()), OperatorLimits::default());
    #[cfg(unix)]
    let module = supervised_privileged_target(&f);
    #[cfg(unix)]
    let before = module.status().unwrap();
    let router = Arc::new(crate::Router::with_control_handler(f.handler.clone()));
    let (mut client, server) = privileged_socket(router).await;
    crate::write_frame(&mut client, &restart(152))
        .await
        .unwrap();
    assert!(matches!(event(&mut events).await, Event::Shown(_)));
    drop(client);
    assert_eq!(
        event(&mut events).await,
        Event::Withdraw("caller_cancelled".into())
    );
    assert_eq!(event(&mut events).await, Event::Returned);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(f.forwarding.operator_confirms().lock().queue.is_empty());
    #[cfg(unix)]
    {
        assert_eq!(
            module.status().unwrap().spawn_generation,
            before.spawn_generation
        );
        assert_eq!(module.status().unwrap().pid, before.pid);
        module.stop().await.unwrap();
    }
}

#[tokio::test]
async fn privileged_disconnect_while_queued_never_shows_that_prompt() {
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
    let f = privileged_fixture(Arc::new(provider.clone()), OperatorLimits::default());
    let router = Arc::new(crate::Router::with_control_handler(f.handler.clone()));
    let (mut first, first_server) = privileged_socket(router.clone()).await;
    let (mut queued, queued_server) = privileged_socket(router).await;
    crate::write_frame(&mut first, &restart(153)).await.unwrap();
    assert!(matches!(event(&mut events).await, Event::Shown(_)));
    crate::write_frame(&mut queued, &restart(154))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.forwarding.operator_confirms().lock().queue.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(queued);
    tokio::time::timeout(Duration::from_secs(2), queued_server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(f.forwarding.operator_confirms().lock().queue.is_empty());
    provider.release();
    assert_eq!(socket_reply(&mut first).await.header.corr, 153);
    assert_eq!(event(&mut events).await, Event::Returned);
    assert!(events.try_recv().is_err());
    drop(first);
    first_server.await.unwrap().unwrap();
}

#[tokio::test]
async fn privileged_applying_rescan_reads_after_approval_and_keeps_enforce_until_restart() {
    let dir = cortexkit_test_support::ScratchDir::new("privileged-rescan");
    let path = dir.join("subc.jsonc");
    std::fs::write(
        &path,
        r#"{"version":1,"privileged_operator_prompt":"enforce"}"#,
    )
    .unwrap();
    let (provider, mut events) =
        Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
    let mut f = privileged_fixture(Arc::new(provider.clone()), OperatorLimits::default());
    let supervisor = crate::Supervisor::new_for_test(
        Arc::new(Registry::default()),
        crate::RestartPolicy::default(),
    )
    .with_handle(f.supervisor.clone());
    f.handler = Arc::new(
        (*f.handler)
            .clone()
            .with_supervisor_rescan(supervisor, &path, None),
    );
    let router = Arc::new(crate::Router::with_control_handler(f.handler.clone()));
    let (mut client, server) = privileged_socket(router).await;
    crate::write_frame(
        &mut client,
        &frame(
            FrameType::Request,
            155,
            serde_json::json!({"op":"supervisor.rescan"}),
        ),
    )
    .await
    .unwrap();
    let Event::Shown(text) = event(&mut events).await else {
        panic!("rescan prompt")
    };
    assert_eq!(text, "Apply the daemon config file as it reads when approved (rescan) — requested by an unidentified local process");
    assert!(f.supervisor.get("edited-after-prompt").is_none());
    std::fs::write(&path, r#"{"version":1,"privileged_operator_prompt":"log","modules":{"edited-after-prompt":{"program":"never-executed","enabled":false}}}"#).unwrap();
    provider.release();
    let answer = socket_reply(&mut client).await;
    assert_eq!(
        body(&answer)["restart_required"],
        serde_json::json!(["privileged_operator_prompt"])
    );
    assert!(
        f.supervisor.get("edited-after-prompt").is_some(),
        "the approved rescan must apply the edited file"
    );
    assert_eq!(event(&mut events).await, Event::Returned);
    crate::write_frame(&mut client, &restart(156))
        .await
        .unwrap();
    assert!(
        matches!(event(&mut events).await, Event::Shown(text) if text.starts_with("Restart module held-target"))
    );
    assert_eq!(socket_reply(&mut client).await.header.corr, 156);
    assert_eq!(event(&mut events).await, Event::Returned);
    drop(client);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn privileged_log_mode_runs_every_classified_write_and_logs_once() {
    let f = Fixture::new(
        Arc::new(Immediate(ProviderResult::Declined)),
        OperatorLimits::default(),
    );
    let (client, _rx) = ctx_fn(840);
    for request in [
        serde_json::json!({"op":"supervisor.restart","module_id":"log-target"}),
        serde_json::json!({"op":"supervisor.swap","module_id":"log-target"}),
        serde_json::json!({"op":"supervisor.reload","module_id":"log-target"}),
        serde_json::json!({"op":"supervisor.rescan"}),
        serde_json::json!({"op":"supervisor.set_enabled","module_id":"log-target","enabled":false}),
        serde_json::json!({"op":"supervisor.set_enabled","module_id":"log-target","enabled":true}),
        serde_json::json!({"op":"supervisor.release_reserved","module_id":"log-target"}),
    ] {
        let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::INFO);
        let op = request["op"].as_str().unwrap();
        let replies = f
            .handler
            .handle_control_frame(&client, frame(FrameType::Request, 157, request.clone()))
            .await
            .unwrap();
        assert_eq!(replies.len(), 1);
        assert_ne!(body(&replies[0])["code"], error_codes::OPERATOR_DECLINED);
        assert!(f.forwarding.operator_confirms().lock().prompt.is_none());
        let logs = crate::router::test_log::captured_logs(&logs);
        let lines: Vec<_> = logs
            .lines()
            .filter(|line| line.contains("privileged request would prompt the operator"))
            .collect();
        assert_eq!(lines.len(), 1, "{logs}");
        assert!(lines[0].contains("INFO") && lines[0].contains(op));
        assert!(lines[0].contains("an unidentified local process"));
        if op != "supervisor.rescan" {
            assert!(lines[0].contains("log-target"));
        }
    }
}

#[tokio::test]
async fn privileged_caller_endpoint_records_are_removed_and_never_reused_stale() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let f = privileged_fixture(
        Arc::new(Immediate(ProviderResult::Unavailable)),
        OperatorLimits::default(),
    );
    let handler = (*f.handler)
        .clone()
        .with_caller_lookup_for_test(move |endpoints| {
            observed.lock().unwrap().push(endpoints);
            crate::caller_identity::CallerIdentity::default()
        });
    let router = crate::Router::with_control_handler(Arc::new(handler.clone()));
    let connection = router.begin_connection();
    let (client, _rx) = ctx_fn(connection.id().get());
    let endpoints = (
        "127.0.0.1:41234".parse().unwrap(),
        "127.0.0.1:41235".parse().unwrap(),
    );
    handler.record_caller_endpoints(client.connection_id, Some(endpoints));
    handler
        .handle_control_frame(&client, restart(158))
        .await
        .unwrap();
    drop(connection);
    handler
        .handle_control_frame(&client, restart(159))
        .await
        .unwrap();
    handler.record_caller_endpoints(client.connection_id, Some(endpoints));
    handler.record_caller_endpoints(client.connection_id, None);
    handler
        .handle_control_frame(&client, restart(160))
        .await
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![Some(endpoints), None, None]);
}

#[tokio::test]
async fn privileged_log_names_the_caller_resolved_from_accepted_socket_endpoints() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let _client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (accepted, _) = listener.accept().await.unwrap();
    let f = Fixture::new(
        Arc::new(Immediate(ProviderResult::Declined)),
        OperatorLimits::default(),
    );
    let (client, _rx) = ctx_fn(842);
    f.handler.record_caller_endpoints(
        client.connection_id,
        Some((
            accepted.local_addr().unwrap(),
            accepted.peer_addr().unwrap(),
        )),
    );
    let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::INFO);
    f.handler
        .handle_control_frame(&client, restart(161))
        .await
        .unwrap();
    let logs = crate::router::test_log::captured_logs(&logs);
    let expected = format!("pid {} (", std::process::id());
    assert!(logs.contains(&expected), "{logs}");
    assert!(
        logs.contains(
            std::env::current_exe()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        ),
        "{logs}"
    );
}

#[cfg(feature = "test-support")]
async fn scripted_privileged_restart(result: &str, expected_code: &str, expected_reason: &str) {
    let dir = cortexkit_test_support::ScratchDir::new("privileged-script");
    let script = dir.join("script");
    let events = dir.join("events");
    std::fs::write(&script, result).unwrap();
    let mut config = crate::bootstrap::BootstrapConfig::new(dir.join("connection"), 0);
    config.operator_script = Some(script);
    config.operator_events = Some(events.clone());
    let provider = super::test_provider::select(
        Arc::new(Immediate(ProviderResult::NoPresence)),
        &config,
        Some(std::path::Path::new("ckdev-subc")),
    );
    let f = privileged_fixture(provider, OperatorLimits::default());
    let (client, _rx) = ctx_fn(850);
    let answer = decision(held_restart(&f, client, 162)).await;
    assert_eq!(answer.header.corr, 162);
    assert_eq!(body(&answer)["code"], expected_code);
    if !expected_reason.is_empty() {
        assert_reason(&answer, expected_reason);
    }
    let events: Vec<serde_json::Value> = std::fs::read_to_string(events)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event"], "prompt_shown");
    assert_eq!(
        events[0]["text"],
        "Restart module held-target — requested by an unidentified local process"
    );
    assert_eq!(events[1]["event"], "provider_returned");
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn privileged_scripted_approval_dispatches_the_original_restart() {
    scripted_privileged_restart("approve", "unknown_module", "").await;
}
#[cfg(feature = "test-support")]
#[tokio::test]
async fn privileged_scripted_decline_refuses_restart() {
    scripted_privileged_restart("decline", error_codes::OPERATOR_DECLINED, "person").await;
}
#[cfg(feature = "test-support")]
#[tokio::test]
async fn privileged_scripted_unavailable_refuses_restart() {
    scripted_privileged_restart(
        "unavailable",
        error_codes::OPERATOR_PRESENCE_UNAVAILABLE,
        "provider_error",
    )
    .await;
}
