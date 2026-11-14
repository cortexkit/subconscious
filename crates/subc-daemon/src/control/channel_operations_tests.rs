//! One test per row of the registration operations table in
//! `channel_operations.rs`, each driven through a real server connection on a
//! supervised spawn's registration channel in its current, candidate and
//! draining roles, plus the activity-read rules for channels and claims.

use super::channel_operations::{
    CLAIM_OPERATION_NOT_ALLOWED, REGISTRATION_CHANNEL_CLOSED, REGISTRATION_OPERATIONS,
};
use super::claims::tests::{
    mint, net_mint, network_primary, network_primary_with_hello, prefix_hello, prefix_wire,
    request, rig, rig_with, rig_with_module, swap_candidate, Net, Rig, SwapRig, Wire,
};
use super::*;
use crate::scopes::LaunchStanding;
use crate::supervise::{
    ModuleOverlap, ModuleSpec, RegistrationSpawn, RestartPolicy, SupervisedModule, Supervisor,
};
use serde_json::{json, Value};
use subc_control::{ClaimMintRequest, ClaimPresentRequest, ClaimPurpose};
use subc_protocol::scope::{RouteAuthorizeExpected, RouteAuthorizeScope, ScopeKind};
use tokio::sync::{mpsc, oneshot};

/// Correlation id `request` puts on every frame.
const CORR: u64 = 7;

fn reply_text(reply: &Frame) -> String {
    String::from_utf8_lossy(&reply.body).into_owned()
}

fn code(reply: &Frame) -> String {
    assert_eq!(reply.header.ty, FrameType::Error, "{}", reply_text(reply));
    serde_json::from_slice::<ErrorBody>(&reply.body)
        .unwrap()
        .code
}

fn ok(reply: &Frame) -> Value {
    assert_eq!(
        reply.header.ty,
        FrameType::Response,
        "{}",
        reply_text(reply)
    );
    serde_json::from_slice(&reply.body).unwrap()
}

/// Send one request and return its answer, skipping frames the daemon sends
/// the connection on its own, such as route binds and the drain notice.
async fn ask(net: &mut Net, body: impl Serialize) -> Frame {
    net.send(request(body)).await;
    answer(net).await
}

/// The answer to the request already sent with [`CORR`].
async fn answer(net: &mut Net) -> Frame {
    loop {
        let frame = net.recv().await.expect("the connection must answer");
        if frame.header.corr == CORR
            && frame.header.channel == 0
            && matches!(frame.header.ty, FrameType::Response | FrameType::Error)
        {
            return frame;
        }
    }
}

/// A consumer connection bound by a claim the primary minted.
async fn consumer(rig: &Rig, primary: &mut Net, purpose: ClaimPurpose) -> Net {
    let claim = net_mint(primary, purpose).await.claim;
    let mut net = Net::new(rig.router.clone()).await;
    ok(&net.call(request(ClaimPresentRequest::new(claim))).await);
    net
}

/// Promote the swap candidate and leave the incumbent draining. The returned
/// sender holds the drain open until it is dropped.
async fn cut_over(rig: &Rig, swap: &mut SwapRig) -> oneshot::Sender<()> {
    let release = rig.handler.forwarding.hold_route_closing_for_test();
    ok(&ask(
        &mut swap.candidate,
        json!({"op":"catalog.update","provides":[],"ready":true}),
    )
    .await);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), &mut swap.swap)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        crate::supervise::SwapOutcome::Cutover
    );
    loop {
        let frame = swap.incumbent.recv().await.expect("drain notice");
        let body: Value = serde_json::from_slice(&frame.body).unwrap_or_default();
        if body["op"] == "module.draining" {
            break;
        }
    }
    release
}

/// Answer a request for `connection` on a fresh sink, as for a frame its
/// reader admitted just before retirement fenced the connection's own sink.
async fn replay(rig: &Rig, connection: ConnectionId, body: impl Serialize) -> Frame {
    let (sender, _replies) = mpsc::channel(8);
    let ctx = RouteCtx {
        connection_id: connection,
        egress: crate::FrameSink::new(sender),
    };
    let mut replies = rig
        .handler
        .handle_control_frame(&ctx, request(body))
        .await
        .unwrap();
    assert_eq!(replies.len(), 1, "{replies:?}");
    replies.pop().unwrap()
}

/// Answer a request on the fixture's primary channel after its spawn was seen
/// to exit.
async fn after_exit(rig: &Rig, body: impl Serialize) -> Frame {
    rig.spawn.retire();
    assert!(!rig.spawn.launch_ready());
    replay(rig, rig.primary.ctx.connection_id, body).await
}

fn hello(module_id: &str) -> Frame {
    let hello = subc_protocol::ModuleHelloBody {
        manifest: ModuleManifest::builder(module_id, "1.0.0").build(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        CORR,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn channel_operations_hello_row_admits_one_hello_per_channel() {
    let rig = rig().await;
    let mut swap = swap_candidate(&rig, "roles-hello").await;
    // Both channels already sent their one HELLO in the fixture.
    for net in [&mut swap.incumbent, &mut swap.candidate] {
        assert_eq!(
            code(&net.call(hello("roles-hello")).await),
            "registration_channel_hello_repeated"
        );
    }
    let _release = cut_over(&rig, &mut swap).await;
    swap.incumbent.send(hello("roles-hello")).await;
    loop {
        let frame = swap.incumbent.recv().await.expect("HELLO refusal");
        if frame.header.ty == FrameType::Error {
            assert_eq!(code(&frame), "registration_channel_hello_repeated");
            break;
        }
    }
}

#[tokio::test]
async fn channel_operations_config_row_serves_current_and_candidate_not_draining() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!("channel-config-roles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(path.join("cortexkit")).unwrap();
    std::fs::write(path.join("subc.jsonc"), br#"{"version":1,"modules":{}}"#).unwrap();
    let owner = Arc::new(crate::config_gate::runtime::RuntimeOwner::new(
        "channel-config-roles".into(),
        crate::config_store::ConfigStore::open_digest_only_for_tests(
            crate::config_store::StoreRoot::from_service_definition(path.join("store")).unwrap(),
        ),
        crate::config_gate::capture::ConfigCapturer {
            daemon_path: path.join("subc.jsonc"),
            config_home: path.clone(),
            projects: Arc::new(crate::config_gate::capture::UnavailableProjectRegistry),
        },
    ));
    let rig = rig_with(move |handler| handler.with_config_runtime(owner)).await;
    let mut swap = swap_candidate(&rig, "roles-config").await;
    // Without a declaration a served connection is refused by the declaration
    // order check, which runs only after the delivery-target role check.
    for net in [&mut swap.incumbent, &mut swap.candidate] {
        assert_eq!(
            code(&ask(net, json!({"op":"config.get"})).await),
            "declaration_order"
        );
        assert_eq!(
            code(&ask(net, json!({"op":"config.applied"})).await),
            "invalid_control_body"
        );
    }
    let _release = cut_over(&rig, &mut swap).await;
    for op in ["config.get", "config.applied"] {
        assert_eq!(
            code(&ask(&mut swap.incumbent, json!({ "op": op })).await),
            "not_delivery_target",
            "{op}"
        );
    }
    assert_eq!(
        code(&ask(&mut swap.candidate, json!({"op":"config.get"})).await),
        "declaration_order",
        "the promoted candidate is the current delivery target"
    );
    drop(swap);
    drop(rig);
    let _ = std::fs::remove_dir_all(path);
}

#[tokio::test]
async fn channel_operations_catalog_and_activity_record_rows_serve_every_live_role() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!("channel-record-roles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let ledger = root.clone();
    let rig = rig_with(move |handler| handler.with_activity_ledger(&ledger, "roles".into())).await;
    let mut swap = swap_candidate(&rig, "roles-record").await;
    let update = json!({"op":"catalog.update","provides":[],"ready":false});
    let declarations = operation_body("catalog.update_role_declarations");
    let record =
        json!({"op":"activity.record","kind":"roles-record.settled","summary":"Step done"});
    ok(&ask(&mut swap.incumbent, &update).await);
    ok(&ask(&mut swap.incumbent, &declarations).await);
    ok(&ask(&mut swap.incumbent, &record).await);
    ok(&ask(&mut swap.candidate, &update).await);
    ok(&ask(&mut swap.candidate, &declarations).await);
    ok(&ask(&mut swap.candidate, &record).await);
    let _release = cut_over(&rig, &mut swap).await;
    ok(&ask(&mut swap.incumbent, &update).await);
    ok(&ask(&mut swap.incumbent, &declarations).await);
    ok(&ask(&mut swap.incumbent, &record).await);
    drop(swap);
    drop(rig);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn channel_operations_claim_mint_row_serves_every_live_role() {
    let rig = rig().await;
    let mut swap = swap_candidate(&rig, "roles-mint").await;
    let body = ClaimMintRequest::new(ClaimPurpose::SelfConnection);
    ok(&ask(&mut swap.incumbent, &body).await);
    ok(&ask(&mut swap.candidate, &body).await);
    let _release = cut_over(&rig, &mut swap).await;
    ok(&ask(&mut swap.incumbent, &body).await);
}

struct Approve;

impl crate::operator_confirm::OperatorProvider for Approve {
    fn prompt(
        &self,
        _: &str,
        _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
    ) -> crate::operator_confirm::ProviderResult {
        crate::operator_confirm::ProviderResult::Approved
    }
}

/// Bind a route from a synthetic client to `module`'s active endpoint, which
/// `module_connection` serves, and return its module-side channel and epoch.
fn open_route(
    rig: &Rig,
    module_connection: ConnectionId,
    module: &str,
    client: u64,
) -> (u16, u32, mpsc::Receiver<crate::router::OutboundFrame>) {
    let (sender, replies) = mpsc::channel(8);
    let pending = rig
        .handler
        .forwarding
        .begin_route_bind_relay_for_test(
            ConnectionId::new(client),
            crate::FrameSink::new(sender),
            2,
            module,
        )
        .unwrap();
    rig.handler
        .forwarding
        .complete_pending_relay(
            module_connection,
            pending.corr,
            crate::forwarding::RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    (pending.module_channel, pending.module_epoch, replies)
}

#[tokio::test]
async fn channel_operations_operator_confirm_row_proves_live_channels_only() {
    let mut rig = rig().await;
    rig.handler.forwarding.operator_confirms().configure(
        Arc::new(Approve),
        crate::operator_confirm::OperatorLimits::default(),
    );
    let self_claim = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    let mut bound = Net::new(rig.router.clone()).await;
    ok(&bound
        .call(request(ClaimPresentRequest::new(self_claim)))
        .await);

    let mut swap = swap_candidate(&rig, "roles-confirm").await;
    let (channel, epoch, _client) = open_route(&rig, swap.incumbent_id, "roles-confirm", 900_001);
    let confirm = OperatorConfirmRequest::new("Reset the cache", channel, epoch);
    assert_eq!(
        ok(&ask(&mut swap.incumbent, &confirm).await)["outcome"],
        "confirmed",
        "the current spawn's channel proves the module"
    );
    let _release = cut_over(&rig, &mut swap).await;
    assert_eq!(
        ok(&ask(&mut swap.incumbent, &confirm).await)["outcome"],
        "confirmed",
        "the draining incumbent's channel still proves the module"
    );
    let (channel, epoch, _promoted_client) =
        open_route(&rig, swap.candidate_id, "roles-confirm", 900_002);
    assert_eq!(
        ok(&ask(
            &mut swap.candidate,
            &OperatorConfirmRequest::new("Reset the cache", channel, epoch)
        )
        .await)["outcome"],
        "confirmed",
        "the promoted candidate's channel proves the module"
    );

    assert_eq!(
        code(&ask(&mut bound, &confirm).await),
        "claim_operation_not_allowed"
    );
    // A module that registered with neither a channel nor a launch nonce
    // proves nothing about who it is.
    let mut unsupervised = Net::new(rig.router.clone()).await;
    assert_eq!(
        unsupervised
            .call(hello("roles-unsupervised"))
            .await
            .header
            .ty,
        FrameType::HelloAck
    );
    let peer = unsupervised.connection_id(&rig.handler);
    let (channel, epoch, _plain_client) = open_route(&rig, peer, "roles-unsupervised", 900_003);
    assert_eq!(
        code(
            &ask(
                &mut unsupervised,
                &OperatorConfirmRequest::new("Reset the cache", channel, epoch)
            )
            .await
        ),
        error_codes::OPERATOR_REQUEST_NOT_PERMITTED
    );
    assert_eq!(
        code(&after_exit(&rig, &confirm).await),
        "registration_channel_closed"
    );
}

fn sync_request(generation: u64) -> ModuleControlRequestFromModule {
    ModuleControlRequestFromModule::ScopeSync {
        generation,
        scopes: vec![ScopeRecord::new("s", 1, ScopeKind::Head)],
    }
}

fn apply_request(generation: u64) -> ModuleControlRequestFromModule {
    ModuleControlRequestFromModule::ScopeApply {
        generation,
        upsert: Vec::new(),
        end: Vec::new(),
    }
}

#[tokio::test]
async fn channel_operations_scope_sync_row_is_keyed_by_the_serving_spawn() {
    let rig = rig().await;
    let mut swap = swap_candidate(&rig, "roles-scope").await;
    let mut bound = consumer(&rig, &mut swap.incumbent, ClaimPurpose::SelfConnection).await;

    // Before anything holds authority, the warming candidate is refused
    // because it is not current, not because another connection holds it.
    let refused = ask(&mut swap.candidate, sync_request(100)).await;
    assert_eq!(code(&refused), error_codes::SCOPE_SYNC_NOT_AUTHORITY);
    assert!(
        reply_text(&refused).contains("not the owner's current supervised launch"),
        "{}",
        reply_text(&refused)
    );

    // The current spawn's channel holds authority: a full sync comes first,
    // and generations only move forward.
    assert_eq!(
        code(&ask(&mut swap.incumbent, apply_request(1)).await),
        error_codes::SCOPE_SYNC_REQUIRED
    );
    ok(&ask(&mut swap.incumbent, sync_request(5)).await);
    assert_eq!(
        code(&ask(&mut swap.incumbent, apply_request(5)).await),
        error_codes::SCOPE_SYNC_STALE
    );
    ok(&ask(&mut swap.incumbent, apply_request(6)).await);

    // The warming candidate and the launch's claim-bound consumer are refused.
    assert_eq!(
        code(&ask(&mut swap.candidate, sync_request(100)).await),
        error_codes::SCOPE_SYNC_NOT_AUTHORITY
    );
    assert_eq!(
        code(&ask(&mut bound, sync_request(100)).await),
        CLAIM_OPERATION_NOT_ALLOWED
    );
    ok(&ask(&mut swap.incumbent, sync_request(7)).await);

    // After cutover the draining incumbent is refused, although its spawn is
    // still alive, and the promoted spawn takes authority at any generation.
    let _release = cut_over(&rig, &mut swap).await;
    for request in [sync_request(8), apply_request(8)] {
        assert_eq!(
            code(&ask(&mut swap.incumbent, request).await),
            error_codes::SCOPE_SYNC_NOT_AUTHORITY
        );
    }
    ok(&ask(&mut swap.candidate, sync_request(1)).await);
    ok(&ask(&mut swap.candidate, apply_request(2)).await);
}

#[tokio::test]
async fn channel_operations_scope_describe_row_serves_every_live_role() {
    let rig = rig().await;
    let mut swap = swap_candidate(&rig, "roles-describe").await;
    let describe = ModuleControlRequestFromModule::ScopeDescribe {
        owner: Principal::Reserved {
            module_id: "roles-describe".into(),
        },
        scope_ref: "s".into(),
    };
    ok(&ask(&mut swap.incumbent, &describe).await);
    ok(&ask(&mut swap.candidate, &describe).await);
    let _release = cut_over(&rig, &mut swap).await;
    ok(&ask(&mut swap.incumbent, &describe).await);
}

#[tokio::test]
async fn channel_operations_route_authorize_row_serves_only_the_current_spawn() {
    let rig =
        rig_with(|handler| handler.with_federated_scope_owner("roles-authorize".into())).await;
    let mut swap = swap_candidate(&rig, "roles-authorize").await;
    let declare = json!({
        "op":"catalog.update","provides":[],
        "capabilities":{"provides":[subc_protocol::scope::ROUTE_AUTHORIZE_CAPABILITY]},
    });
    ok(&ask(&mut swap.incumbent, &declare).await);
    ok(&ask(&mut swap.candidate, &declare).await);
    let source = ScopeRecord::new("s", 1, ScopeKind::Head);
    let authorize = ModuleControlRequestFromModule::RouteAuthorize {
        principal: Principal::Direct,
        scope: RouteAuthorizeScope::new(
            Principal::Reserved {
                module_id: "roles-authorize".into(),
            },
            "s",
            1,
        ),
        target: "roles-authorize".into(),
        expected: RouteAuthorizeExpected::new(source.kind, &source.attributes),
    };
    assert_eq!(
        ok(&ask(&mut swap.incumbent, &authorize).await)["op"],
        "route.authorize"
    );
    assert_eq!(
        code(&ask(&mut swap.candidate, &authorize).await),
        error_codes::ROUTE_AUTHORIZE_NOT_PERMITTED
    );
    let _release = cut_over(&rig, &mut swap).await;
    assert_eq!(
        code(&ask(&mut swap.incumbent, &authorize).await),
        error_codes::ROUTE_AUTHORIZE_NOT_PERMITTED
    );
    assert_eq!(
        ok(&ask(&mut swap.candidate, &authorize).await)["op"],
        "route.authorize"
    );
}

#[tokio::test]
async fn channel_operations_registration_reads_row_keeps_each_existing_requirement() {
    let rig = rig().await;
    let mut swap = swap_candidate(&rig, "roles-reads").await;
    let live_roots = json!({"op":"supervisor.live_roots"});
    let served_by = subc_control::ScopeServedByRequest::new("s".into(), 1, "tool".into());
    let agents = json!({"op":"supervisor.agent_commands","started":[],"finished":[]});
    for net in [&mut swap.incumbent, &mut swap.candidate] {
        ok(&ask(net, &live_roots).await);
        ok(&ask(net, &served_by).await);
    }
    ok(&ask(&mut swap.incumbent, &agents).await);
    assert_eq!(
        code(&ask(&mut swap.candidate, &agents).await),
        "not_supervised",
        "agent commands come only from the module's current launch"
    );
    let _release = cut_over(&rig, &mut swap).await;
    ok(&ask(&mut swap.incumbent, &live_roots).await);
    ok(&ask(&mut swap.incumbent, &served_by).await);
    assert_eq!(
        code(&ask(&mut swap.incumbent, &agents).await),
        "not_supervised"
    );
    ok(&ask(&mut swap.candidate, &agents).await);
}

fn operation_body(op: &str) -> Value {
    if op == "claim.mint" {
        serde_json::to_value(ClaimMintRequest::new(ClaimPurpose::SelfConnection)).unwrap()
    } else if op == "catalog.update_role_declarations" {
        json!({"op":op,"role_declarations":{}})
    } else {
        json!({ "op": op })
    }
}

/// Every operation a module sends the daemon on channel 0, written out here
/// instead of being read from the daemon's own lists, each marked with whether
/// it needs the module's own registration.
const MODULE_TO_DAEMON_OPERATIONS: &[(&str, bool)] = &[
    ("config.declare", true),
    ("config.get", true),
    ("config.applied", true),
    ("catalog.update", true),
    ("catalog.update_role_declarations", true),
    ("activity.record", true),
    ("claim.mint", true),
    ("operator.confirm", true),
    ("scope.sync", true),
    ("scope.apply", true),
    ("scope.describe", true),
    ("route.authorize", true),
    ("supervisor.live_roots", true),
    ("supervisor.agent_commands", true),
    ("scope.served_by", true),
    // Connection setup, and reads a module makes as a client.
    ("claim.present", false),
    ("activity.query", false),
    ("activity.subscribe", false),
    ("catalog.watch", false),
];

#[test]
fn channel_operations_catalog_watch_is_a_client_read() {
    assert!(MODULE_TO_DAEMON_OPERATIONS.contains(&("catalog.watch", false)));
    assert!(!REGISTRATION_OPERATIONS.contains(&"catalog.watch"));
}

fn registration_operations() -> Vec<&'static str> {
    MODULE_TO_DAEMON_OPERATIONS
        .iter()
        .filter(|(_, registration)| *registration)
        .map(|(op, _)| *op)
        .collect()
}

#[test]
fn channel_operations_every_module_operation_is_classified() {
    for op in MODULE_TO_SUBC_CONTROL_OPS
        .iter()
        .chain(MODULE_TO_SUBC_UNADVERTISED_OPS)
    {
        assert!(
            MODULE_TO_DAEMON_OPERATIONS
                .iter()
                .any(|(known, _)| known == op),
            "{op} is a module operation this table does not classify"
        );
    }
    let mut expected = registration_operations();
    let mut gated = REGISTRATION_OPERATIONS.to_vec();
    expected.sort_unstable();
    gated.sort_unstable();
    assert_eq!(
        gated, expected,
        "the registration check covers exactly these"
    );
}

#[tokio::test]
async fn channel_operations_refuse_consumer_claims_and_exited_spawns() {
    let mut rig = rig().await;
    let mut consumers = Vec::new();
    for purpose in [
        ClaimPurpose::SelfConnection,
        ClaimPurpose::Helper,
        ClaimPurpose::Relay,
    ] {
        let claim = mint(&mut rig, purpose).await.claim;
        let mut net = Net::new(rig.router.clone()).await;
        ok(&net.call(request(ClaimPresentRequest::new(claim))).await);
        consumers.push(net);
    }
    // `claim.mint` is dispatched before the registration check and refuses
    // both kinds of connection with its own code.
    for op in registration_operations() {
        for net in &mut consumers {
            let expected = if op == "claim.mint" {
                error_codes::CLAIM_MINT_NOT_ALLOWED
            } else {
                CLAIM_OPERATION_NOT_ALLOWED
            };
            assert_eq!(code(&ask(net, operation_body(op)).await), expected, "{op}");
        }
    }
    for op in registration_operations() {
        let expected = if op == "claim.mint" {
            error_codes::CLAIM_MINT_NOT_ALLOWED
        } else {
            REGISTRATION_CHANNEL_CLOSED
        };
        assert_eq!(
            code(&after_exit(&rig, operation_body(op)).await),
            expected,
            "{op}"
        );
    }
}

#[tokio::test]
async fn channel_operations_role_declarations_share_catalog_update_refusals() {
    let mut rig = rig().await;
    let claim = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    let mut net = Net::new(rig.router.clone()).await;
    ok(&net.call(request(ClaimPresentRequest::new(claim))).await);
    let catalog = json!({"op":"catalog.update","provides":[]});
    let declarations = operation_body("catalog.update_role_declarations");
    assert_eq!(
        code(&ask(&mut net, &catalog).await),
        CLAIM_OPERATION_NOT_ALLOWED
    );
    assert_eq!(
        code(&ask(&mut net, &declarations).await),
        CLAIM_OPERATION_NOT_ALLOWED
    );
    assert_eq!(
        code(&after_exit(&rig, &catalog).await),
        REGISTRATION_CHANNEL_CLOSED
    );
    assert_eq!(
        code(&after_exit(&rig, &declarations).await),
        REGISTRATION_CHANNEL_CLOSED
    );
}

/// A fixture whose handler keeps an activity ledger.
async fn activity_rig(label: &str) -> (Rig, std::path::PathBuf) {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!("channel-activity-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let ledger = root.clone();
    let rig =
        rig_with(move |handler| handler.with_activity_ledger(&ledger, "activity".into())).await;
    (rig, root)
}

fn query(consumer_identity: Option<Value>) -> Value {
    let mut body = json!({"op":"activity.query","limit":10});
    if let Some(identity) = consumer_identity {
        body["consumer_identity"] = identity;
    }
    body
}

#[tokio::test]
async fn channel_operations_activity_reads_take_the_principal_from_the_binding() {
    let (mut rig, root) = activity_rig("principal").await;
    let (mut callosum, _spawn, _module, _) = network_primary(&rig, "callosum").await;
    let callosum_nonce = rig
        .handler
        .supervisor
        .spawn_launch_nonce_for("callosum")
        .expect("an ungated spawn also gets a launch nonce");
    let borrowed = json!({"module_id":"callosum","launch_nonce":callosum_nonce});
    let forbidden = subc_control::ACTIVITY_READ_FORBIDDEN;

    // A `self` connection of an ordinary module reads as that module, which is
    // not an activity reader, whatever its body claims to be.
    let own = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    let mut ordinary = Net::new(rig.router.clone()).await;
    ok(&ordinary.call(request(ClaimPresentRequest::new(own))).await);
    assert_eq!(code(&ask(&mut ordinary, query(None)).await), forbidden);
    assert_eq!(
        code(&ask(&mut ordinary, query(Some(borrowed.clone()))).await),
        forbidden,
        "a body identity cannot replace the binding"
    );

    // A channel that has not sent HELLO yet is still its spawn's binding.
    let (mut unregistered, _unregistered_spawn, _unregistered_module, _) =
        network_primary_with_hello(&rig, "roles-unregistered", false).await;
    assert_eq!(code(&ask(&mut unregistered, query(None)).await), forbidden);

    // Helper and relay connections act for someone else, so even an elevated
    // reader's are refused instead of being treated as `direct`.
    for purpose in [ClaimPurpose::Helper, ClaimPurpose::Relay] {
        let mut net = consumer(&rig, &mut callosum, purpose).await;
        assert_eq!(code(&ask(&mut net, query(None)).await), forbidden);
    }

    // The elevated reader keeps its standing on its live channel and on its
    // `self` connections, which ignore a body identity naming anyone else.
    let mut reader = consumer(&rig, &mut callosum, ClaimPurpose::SelfConnection).await;
    let stranger = json!({"module_id":"claims-module","launch_nonce":"not-a-nonce"});
    ok(&ask(&mut reader, query(Some(stranger))).await);
    ok(&ask(&mut callosum, query(None)).await);

    // A connection without a binding keeps the nonce-era rules: `direct`
    // reads, and so does a consumer proving the ungated reader's nonce.
    let mut plain = Net::new(rig.router.clone()).await;
    ok(&ask(&mut plain, query(None)).await);
    ok(&ask(&mut plain, query(Some(borrowed))).await);

    drop((ordinary, unregistered, reader, plain, callosum));
    drop(rig);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn channel_operations_claim_subscriptions_end_when_the_binding_is_revoked() {
    let (rig, root) = activity_rig("revoked").await;
    let (mut callosum, spawn, _module, _) = network_primary(&rig, "callosum").await;
    let claim = net_mint(&mut callosum, ClaimPurpose::SelfConnection)
        .await
        .claim;
    // An in-process connection: nothing here processes its close request, so
    // only the binding's revocation can end the subscription.
    let mut reader = Wire::new(&rig.router);
    ok(&reader
        .call(&rig.router, request(ClaimPresentRequest::new(claim)))
        .await);
    let subscribe = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        41,
        serde_json::to_vec(&json!({"op":"activity.subscribe"})).unwrap(),
    )
    .unwrap();
    rig.router
        .route_for_connection(&reader.ctx, subscribe)
        .await
        .unwrap();
    let key = (reader.ctx.connection_id, 41);
    let running = |handler: &ControlHandler| {
        handler
            .activity_streams
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|task| !task.is_finished())
    };
    assert!(running(&rig.handler), "the subscription must be admitted");
    spawn.retire();
    tokio::time::timeout(Duration::from_secs(2), async {
        while running(&rig.handler) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("revoking the binding must end its subscription");
    drop((reader, callosum));
    drop(rig);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn channel_operations_prefix_registration_dies_with_its_owner() {
    let (mut rig, root) = activity_rig("prefix").await;
    let mut child = prefix_wire(&mut rig, "claims:roles-child").await;
    assert_eq!(
        child
            .call(&rig.router, prefix_hello("claims:roles-child"))
            .await
            .header
            .ty,
        FrameType::HelloAck
    );
    let child_id = child.ctx.connection_id;
    let brokered = [
        json!({"op":"catalog.update","provides":[],"ready":false}),
        operation_body("catalog.update_role_declarations"),
        json!({"op":"activity.record","kind":"claims:roles-child.settled","summary":"Settled"}),
        json!({"op":"scope.describe","owner":{"kind":"reserved","module_id":"claims-module"},"ref":"missing"}),
        json!({"op":"supervisor.live_roots"}),
    ];
    for body in &brokered {
        ok(&replay(&rig, child_id, body).await);
    }
    // The owner exits. Its prefix child's registry entry and binding remain
    // until the connection's cleanup runs, but a frame read before the claim
    // connection was fenced is refused.
    rig.spawn.retire();
    for body in &brokered {
        assert_eq!(
            code(&replay(&rig, child_id, body).await),
            REGISTRATION_CHANNEL_CLOSED,
            "{body}"
        );
    }
    drop(child);
    drop(rig);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_operations_busy_launch_record_is_retryable_for_the_serving_spawn() {
    let rig = rig().await;
    let (mut net, _spawn, module, _) = network_primary(&rig, "roles-busy").await;
    let agents = json!({"op":"supervisor.agent_commands","started":[],"finished":[]});
    let (locked, wait_locked) = std::sync::mpsc::channel();
    let (release, wait_release) = std::sync::mpsc::channel::<()>();
    let held = module.clone();
    let holder = std::thread::spawn(move || {
        held.with_resource_snapshot_locked_for_test(|| {
            locked.send(()).unwrap();
            let _ = wait_release.recv_timeout(Duration::from_secs(5));
        })
    });
    wait_locked.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(code(&ask(&mut net, sync_request(1)).await), "busy");
    assert_eq!(code(&ask(&mut net, &agents).await), "busy");
    release.send(()).unwrap();
    holder.join().unwrap();
    ok(&ask(&mut net, sync_request(1)).await);
    ok(&ask(&mut net, &agents).await);
}

#[tokio::test]
async fn channel_operations_cutover_interval_is_retryable_for_the_promoted_spawn() {
    let rig = rig().await;
    let mut swap = swap_candidate(&rig, "roles-cutover").await;
    // Publish what a cutover publishes first, the forwarding endpoint and the
    // registry slot, and stop before the supervisor rewrites the launch
    // record, which still describes the incumbent's process.
    rig.handler
        .forwarding
        .cutover_candidate("roles-cutover")
        .unwrap()
        .expect("the candidate's endpoint takes over");
    rig.handler
        .registry
        .promote_candidate("roles-cutover")
        .unwrap()
        .expect("the registry names the candidate");
    let agents = json!({"op":"supervisor.agent_commands","started":[],"finished":[]});
    assert_eq!(
        code(&ask(&mut swap.candidate, sync_request(1)).await),
        "busy"
    );
    assert_eq!(code(&ask(&mut swap.candidate, &agents).await), "busy");
    assert_eq!(
        code(&ask(&mut swap.incumbent, sync_request(1)).await),
        error_codes::SCOPE_SYNC_NOT_AUTHORITY
    );
    assert_eq!(
        code(&ask(&mut swap.incumbent, &agents).await),
        "not_supervised"
    );
}

#[tokio::test]
async fn channel_operations_exited_reader_channel_and_claims_read_nothing() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!("channel-activity-exited-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let ledger = root.clone();
    let mut rig = rig_with_module("callosum", move |handler| {
        handler.with_activity_ledger(&ledger, "activity".into())
    })
    .await;
    let claim = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    let mut reader = Wire::new(&rig.router);
    ok(&reader
        .call(&rig.router, request(ClaimPresentRequest::new(claim)))
        .await);
    let readers = [rig.primary.ctx.connection_id, reader.ctx.connection_id];
    for connection in readers {
        ok(&replay(&rig, connection, query(None)).await);
    }
    rig.spawn.retire();
    for connection in readers {
        assert_eq!(
            code(&replay(&rig, connection, query(None)).await),
            subc_control::ACTIVITY_READ_FORBIDDEN
        );
    }
    drop(reader);
    drop(rig);
    let _ = std::fs::remove_dir_all(root);
}

/// A warming candidate serves no route before cutover, and `operator.confirm`
/// names a route its sender serves, so a candidate's confirm cannot succeed
/// end to end. Check the channel proof it would carry directly.
#[tokio::test]
async fn channel_operations_warming_candidate_channel_proves_operator_confirm() {
    let rig = rig().await;
    let swap = swap_candidate(&rig, "roles-confirm-candidate").await;
    for connection in [swap.candidate_id, swap.incumbent_id] {
        assert_eq!(
            rig.handler
                .confirm_channel_proven(connection, "roles-confirm-candidate"),
            Some(true)
        );
    }
    assert_eq!(
        rig.handler
            .confirm_channel_proven(swap.candidate_id, "another-module"),
        Some(false)
    );
    rig.spawn.retire();
    assert_eq!(
        rig.handler
            .confirm_channel_proven(rig.primary.ctx.connection_id, "claims-module"),
        Some(false)
    );
}

/// A supervised module's registration channel bound to an in-process
/// connection that has not sent HELLO, and the launch nonce its ungated spawn
/// also received.
async fn unregistered_channel(
    rig: &Rig,
    module_id: &str,
) -> (Wire, Arc<RegistrationSpawn>, SupervisedModule, String) {
    let handle = rig.handler.supervisor.clone();
    let supervisor =
        Supervisor::new_for_test(rig.handler.registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_forwarding(rig.handler.forwarding.clone());
    let module = supervisor
        .spawn_async(ModuleSpec {
            module_id: module_id.into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "exec sleep 300".into()],
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: subc_control::ModuleProtocol::Subc,
            overlap: ModuleOverlap::Safe,
            carry_containment: false,
        })
        .await
        .unwrap();
    let spawn = handle.registration_for_test(module_id).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(6), spawn.wait_launch())
        .await
        .expect("registration launch must settle");
    let nonce = handle
        .spawn_launch_nonce_for(module_id)
        .expect("an ungated spawn also gets a launch nonce");
    let wire = Wire::new(&rig.router);
    spawn.bind(wire.ctx.connection_id);
    (wire, spawn, module, nonce)
}

fn hello_with_nonce(module_id: &str, nonce: &str) -> Frame {
    let body = subc_protocol::ModuleHelloBody {
        manifest: ModuleManifest::builder(module_id, "1.0.0").build(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: Some(nonce.into()),
    };
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        CORR,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

/// [`unregistered_channel`] after a HELLO that also presents the nonce.
async fn channel_with_nonce(
    rig: &Rig,
    module_id: &str,
) -> (Wire, Arc<RegistrationSpawn>, SupervisedModule, String) {
    let (mut wire, spawn, module, nonce) = unregistered_channel(rig, module_id).await;
    let ack = wire
        .call(&rig.router, hello_with_nonce(module_id, &nonce))
        .await;
    assert_eq!(ack.header.ty, FrameType::HelloAck, "{}", reply_text(&ack));
    (wire, spawn, module, nonce)
}

fn agent_commands() -> Value {
    json!({"op":"supervisor.agent_commands","started":[],"finished":[]})
}

#[tokio::test]
async fn channel_operations_channel_never_authenticates_through_a_presented_nonce() {
    let rig = rig().await;
    let (wire, _spawn, _module, nonce) = channel_with_nonce(&rig, "roles-nonce").await;
    let connection = wire.ctx.connection_id;
    assert_eq!(
        rig.handler
            .connection_is_current_launch("roles-nonce", connection, Some(&nonce)),
        LaunchStanding::Current
    );
    // Cleanup forgets the channel before it deregisters the connection. In
    // that interval the nonce the HELLO also carried must not stand in.
    rig.handler
        .supervisor
        .forget_registration_connection(connection);
    assert_eq!(
        rig.handler
            .connection_is_current_launch("roles-nonce", connection, Some(&nonce)),
        LaunchStanding::NotCurrent
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_operations_declaration_running_during_cleanup_is_refused() {
    let rig = rig().await;
    let (wire, _spawn, _module, _nonce) = channel_with_nonce(&rig, "roles-queued").await;
    let connection = wire.ctx.connection_id;
    // Hold the declaration at its commit fence: authorized, with its fence
    // registered in the queue. Everything until its release happens well
    // inside the queue's validation window, so its own timeout cannot be what
    // refuses it. Its reply goes to a sink that retirement does not close.
    let (reached, resume) = rig.handler.supervisor.pause_resource_commit_for_test();
    let (sender, mut replies) = mpsc::channel(8);
    let ctx = RouteCtx {
        connection_id: connection,
        egress: crate::FrameSink::new(sender),
    };
    let declaration = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        51,
        serde_json::to_vec(&agent_commands()).unwrap(),
    )
    .unwrap();
    assert!(rig
        .handler
        .handle_control_frame(&ctx, declaration)
        .await
        .unwrap()
        .is_empty());
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .expect("the declaration must reach its commit fence")
        .unwrap();

    // Cleanup closes the declaration queue and is held there, before it
    // retires the spawn: the channel is still live and serving, so only the
    // cancelled fence can stop the commit.
    let (cleanup_reached_sender, cleanup_reached) = oneshot::channel();
    let (cleanup_release, cleanup_wait) = std::sync::mpsc::channel();
    *rig.handler.cleanup_pause.lock().unwrap() = Some((0, (cleanup_reached_sender, cleanup_wait)));
    let handler = rig.handler.clone();
    let cleanup =
        tokio::task::spawn_blocking(move || handler.cleanup_connection(connection).map(|_| ()));
    tokio::time::timeout(Duration::from_secs(5), cleanup_reached)
        .await
        .expect("cleanup must close the declaration queue")
        .unwrap();
    resume.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv())
        .await
        .expect("the declaration must be answered")
        .unwrap()
        .frame;
    assert_eq!(reply.header.corr, 51);
    assert_eq!(code(&reply), "busy", "{}", reply_text(&reply));
    assert_eq!(
        serde_json::from_slice::<ErrorBody>(&reply.body)
            .unwrap()
            .message,
        "resource state is busy after revalidation; retry uncompleted finished and known entries",
        "the cancelled commit fence, not a timeout, must refuse the declaration"
    );
    cleanup_release.send(()).unwrap();
    cleanup.await.unwrap().unwrap();
    drop(wire);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_operations_hello_outliving_cleanup_records_no_nonce_and_never_registers() {
    let rig = rig().await;
    let (wire, _spawn, _module, nonce) = unregistered_channel(&rig, "roles-late-hello").await;
    let connection = wire.ctx.connection_id;
    // The reader resolves the channel and is held before the blocking HELLO
    // handler starts, as when the HELLO outlives its reader.
    let (reached_sender, reached) = oneshot::channel();
    let (release, wait_release) = oneshot::channel();
    *rig.handler.hello_ingress_pause.lock().unwrap() = Some((reached_sender, wait_release));
    let router = rig.router.clone();
    let ctx = wire.ctx.clone();
    let hello = hello_with_nonce("roles-late-hello", &nonce);
    let hello = tokio::spawn(async move { router.route_for_connection(&ctx, hello).await });
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .expect("the HELLO must resolve its channel")
        .unwrap();
    // Cleanup retires the spawn and forgets the channel, and is held there.
    let (cleanup_reached_sender, cleanup_reached) = oneshot::channel();
    let (cleanup_release, cleanup_wait) = std::sync::mpsc::channel();
    *rig.handler.cleanup_pause.lock().unwrap() = Some((1, (cleanup_reached_sender, cleanup_wait)));
    let handler = rig.handler.clone();
    let cleanup =
        tokio::task::spawn_blocking(move || handler.cleanup_connection(connection).map(|_| ()));
    tokio::time::timeout(Duration::from_secs(5), cleanup_reached)
        .await
        .expect("cleanup must forget the channel")
        .unwrap();
    assert!(rig
        .handler
        .supervisor
        .registration_spawn(connection)
        .is_none());
    release.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), hello)
        .await
        .expect("the HELLO must settle");
    assert!(
        rig.handler
            .hello_launch_nonces
            .lock()
            .unwrap()
            .nonce(connection)
            .is_none(),
        "a channel HELLO recorded a launch nonce after teardown"
    );
    assert!(
        rig.handler
            .registry
            .get_module("roles-late-hello")
            .unwrap()
            .is_none(),
        "a channel HELLO registered after its spawn retired"
    );
    cleanup_release.send(()).unwrap();
    cleanup.await.unwrap().unwrap();
    drop(wire);
}

#[tokio::test]
async fn channel_operations_exited_predecessor_record_is_busy_not_final() {
    let rig = rig().await;
    let (mut net, _spawn, module, _) = network_primary(&rig, "roles-restarted").await;
    // After a restart the new spawn serves before its launch record is
    // rewritten, so the record still describes the exited predecessor. Pid 1
    // is never this spawn's process.
    module.set_launch_exited_predecessor_for_test(1);
    assert_eq!(code(&ask(&mut net, sync_request(1)).await), "busy");
    assert_eq!(code(&ask(&mut net, agent_commands()).await), "busy");
}

#[tokio::test]
async fn channel_operations_stopped_launch_is_not_current_rather_than_busy() {
    let rig = rig().await;
    let (mut net, _spawn, module, _) = network_primary(&rig, "roles-stopping").await;
    ok(&ask(&mut net, sync_request(1)).await);
    // The launch record shows the process gone before the spawn's exit has
    // been observed: the spawn will never be current again.
    module.set_launch_alive_for_test(false);
    assert_eq!(
        code(&ask(&mut net, sync_request(2)).await),
        error_codes::SCOPE_SYNC_NOT_AUTHORITY
    );
    assert_eq!(
        code(&ask(&mut net, agent_commands()).await),
        "not_supervised"
    );
    module.set_launch_alive_for_test(true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_operations_busy_launch_read_after_admission_is_retryable() {
    let rig = rig().await;
    let (mut net, _spawn, module, _) = network_primary(&rig, "roles-second-busy").await;
    // Hold the declaration after its current-launch check, then lock the
    // launch record only for the read that follows.
    let (reached_sender, reached) = oneshot::channel();
    let (release, wait_release) = std::sync::mpsc::channel();
    *rig.handler.agent_commands_pause.lock().unwrap() = Some((1, (reached_sender, wait_release)));
    net.send(request(agent_commands())).await;
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .expect("the declaration must pass its current-launch check")
        .unwrap();
    let (locked, wait_locked) = std::sync::mpsc::channel();
    let (unlock, wait_unlock) = std::sync::mpsc::channel::<()>();
    let held = module.clone();
    let holder = std::thread::spawn(move || {
        held.with_resource_snapshot_locked_for_test(|| {
            locked.send(()).unwrap();
            let _ = wait_unlock.recv_timeout(Duration::from_secs(5));
        })
    });
    wait_locked.recv_timeout(Duration::from_secs(5)).unwrap();
    release.send(()).unwrap();
    assert_eq!(code(&answer(&mut net).await), "busy");
    unlock.send(()).unwrap();
    holder.join().unwrap();
    ok(&ask(&mut net, agent_commands()).await);
}
