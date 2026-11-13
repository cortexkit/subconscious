//! Spawn authority attestation, driven through real server connections: what
//! `route.open` attests about its opener (`caller` on `route.bind`) and its
//! target (`target` on the reply), the epoch fence at bind commit, and the
//! authority changes that close routes. The first-party mark is false on
//! every wire in this file's own tests, and each pins that where it reads an
//! attestation; the nested `first_party_mark` tests raise and lower it.

use super::claims::tests::{
    mint, network_primary, network_primary_with_hello, request, rig, swap_candidate, Net, Rig,
};
use super::*;
use serde_json::{json, Value};
use subc_control::{ClaimPresentRequest, ClaimPurpose};
use subc_protocol::manifest::{
    CapabilityDeclarations, Concurrency, ExecutionMode, IdentityScope, ProviderRole, Tool,
};
use subc_protocol::peer::CALLER_ATTESTATION_CAPABILITY;

const OPEN_CORR: u64 = 41;

#[path = "first_party_mark_tests.rs"]
mod first_party_mark;

#[tokio::test]
async fn origins_widening_fences_routes_and_pending_binds() {
    let rig = rig().await;
    let (mut target, spawn, _module, _) =
        network_primary_with_hello(&rig, "widening-target", false).await;
    spawn.authority.initialize_origin(
        subc_protocol::peer::CallerOrigin::Development,
        Default::default(),
        Some(false),
    );
    let entries = serde_json::from_value(json!({"provider":{"items":["one"]}})).unwrap();
    let mut manifest = tool_manifest("widening-target", &[], &[]);
    manifest.provider_declarations = Some(entries);
    hello(&mut target, manifest).await;
    let mut opener = Net::new(rig.router.clone()).await;
    let (_, reply) = open(&mut opener, &mut target, "widening-target", false).await;
    assert_eq!(reply.header.ty, FrameType::Response);
    let mut pending_opener = Net::new(rig.router.clone()).await;
    pending_opener
        .send(open_frame("widening-target", false, None))
        .await;
    let pending_bind = next_bind(&mut target).await;
    let entries = serde_json::from_value(json!({"provider":{"items":["one"]}})).unwrap();
    rig.handler
        .approve_spawn_declarations(&spawn.authority, &entries);
    assert_eq!(spawn.authority.capture().unwrap().approved.entries, entries);
    assert_eq!(
        rig.handler
            .forwarding
            .published_spawn_epoch(&spawn.authority.key()),
        1
    );
    assert!(rig
        .handler
        .forwarding
        .route_authority_for_test(connection_of(&rig, &opener))
        .is_empty());
    let closed = loop {
        let frame = opener.recv().await.expect("route.closed");
        if frame.header.ty == FrameType::Push && body(&frame)["op"] == "route.closed" {
            break body(&frame);
        }
    };
    let reason: RouteCloseReason = serde_json::from_value(closed["reason"].clone()).unwrap();
    assert!(reason.may_reopen());
    ack(&mut target, &pending_bind).await;
    let refused = open_reply(&mut pending_opener).await;
    assert_eq!(
        body(&refused)["code"],
        subc_protocol::error_codes::STALE_AUTHORITY_EPOCH
    );
    let (_, reply) = open(&mut pending_opener, &mut target, "widening-target", false).await;
    assert_eq!(body(&reply)["target"]["authority_epoch"], 1);
    assert_eq!(
        body(&reply)["target"]["approved_declarations_digest"],
        spawn.authority.capture().unwrap().approved.digest
    );
    assert_eq!(body(&reply)["target"]["first_party"], false);
}

fn text(frame: &Frame) -> String {
    String::from_utf8_lossy(&frame.body).into_owned()
}

fn body(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap_or_else(|_| panic!("{}", text(frame)))
}

/// A routable manifest declaring `provides` capabilities and denying
/// `denies`.
fn tool_manifest(module_id: &str, provides: &[&str], denies: &[&str]) -> ModuleManifest {
    ModuleManifest::builder(module_id, "1.0.0")
        .provides(vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "read".into(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: json!({"type": "object"}),
            }],
            identity_scope: vec![IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: false,
            sub_supervises: false,
        }])
        .capabilities(Some(CapabilityDeclarations {
            provides: provides.iter().map(|cap| cap.to_string()).collect(),
            requires: Vec::new(),
            must_never_reach: denies.iter().map(|cap| cap.to_string()).collect(),
        }))
        .build()
}

fn hello_frame(manifest: ModuleManifest) -> Frame {
    let hello = subc_protocol::ModuleHelloBody {
        manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        3,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap()
}

async fn hello(net: &mut Net, manifest: ModuleManifest) -> Frame {
    let ack = net.call(hello_frame(manifest)).await;
    assert_eq!(ack.header.ty, FrameType::HelloAck, "{}", text(&ack));
    ack
}

/// An unsupervised module serving the tool role, with `capabilities`.
async fn unsupervised_target(rig: &Rig, module_id: &str, capabilities: &[&str]) -> Net {
    let mut net = Net::new(rig.router.clone()).await;
    hello(&mut net, tool_manifest(module_id, capabilities, &[])).await;
    net
}

/// A supervised spawn's own registration serving the tool role.
async fn supervised_target(
    rig: &Rig,
    module_id: &str,
    capabilities: &[&str],
) -> (
    Net,
    Arc<crate::supervise::RegistrationSpawn>,
    crate::supervise::SupervisedModule,
) {
    let (mut net, spawn, module, _) = network_primary_with_hello(rig, module_id, false).await;
    hello(&mut net, tool_manifest(module_id, capabilities, &[])).await;
    (net, spawn, module)
}

/// A connection bound by a claim the fixture's primary registration minted.
async fn claimed(rig: &mut Rig, purpose: ClaimPurpose) -> Net {
    let claim = mint(rig, purpose).await.claim;
    let mut net = Net::new(rig.router.clone()).await;
    let reply = net.call(request(ClaimPresentRequest::new(claim))).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    net
}

fn open_frame(
    module_id: &str,
    busy_refusals: bool,
    consumer_identity: Option<subc_control::ConsumerIdentity>,
) -> Frame {
    open_frame_scoped(module_id, busy_refusals, consumer_identity, None)
}

fn open_frame_scoped(
    module_id: &str,
    busy_refusals: bool,
    consumer_identity: Option<subc_control::ConsumerIdentity>,
    scope: Option<subc_protocol::scope::ScopeSelector>,
) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
        busy_refusals: busy_refusals.then_some(true),
        target: RouteTarget::ToolProvider {
            module_id: module_id.into(),
        },
        identity: BindIdentity::new("attestation".to_string(), "session".to_string()),
        consumer_identity,
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope,
    })
    .unwrap();
    Frame::build(FrameType::Request, control_flags(), 0, 0, OPEN_CORR, body).unwrap()
}

/// The next `route.bind` the target receives, skipping anything else.
async fn next_bind(target: &mut Net) -> Frame {
    loop {
        let frame = target
            .recv()
            .await
            .expect("the target must receive the bind");
        if frame.header.ty == FrameType::Request
            && frame.header.channel == 0
            && body(&frame)["op"] == "route.bind"
        {
            return frame;
        }
    }
}

async fn ack(target: &mut Net, bind: &Frame) {
    let body = serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap();
    target
        .send(
            Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                bind.header.corr,
                body,
            )
            .unwrap(),
        )
        .await;
}

/// The opener's answer to its `route.open`.
async fn open_reply(opener: &mut Net) -> Frame {
    loop {
        let frame = opener.recv().await.expect("the opener must be answered");
        if frame.header.corr == OPEN_CORR
            && frame.header.channel == 0
            && matches!(frame.header.ty, FrameType::Response | FrameType::Error)
        {
            return frame;
        }
    }
}

/// Open a route and let the target accept it: the bind and the reply.
async fn open(
    opener: &mut Net,
    target: &mut Net,
    module_id: &str,
    busy_refusals: bool,
) -> (Value, Frame) {
    opener
        .send(open_frame(module_id, busy_refusals, None))
        .await;
    let bind = next_bind(target).await;
    ack(target, &bind).await;
    let reply = open_reply(opener).await;
    (body(&bind), reply)
}

fn connection_of(rig: &Rig, net: &Net) -> ConnectionId {
    net.connection_id(&rig.handler)
}

async fn catalog_entry(rig: &Rig, module_id: &str) -> Value {
    let mut reader = Net::new(rig.router.clone()).await;
    let reply = reader
        .call(request(
            json!({"op": "catalog.list", "module_id": module_id}),
        ))
        .await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    let modules = body(&reply)["modules"].as_array().unwrap().clone();
    assert_eq!(modules.len(), 1);
    modules[0].clone()
}

#[tokio::test]
async fn relay_seams_record_reply_and_catalog_share_approved_declarations() {
    let rig = rig().await;
    let (mut target, spawn, _module, _) =
        network_primary_with_hello(&rig, "declaration-target", false).await;
    let mut manifest = tool_manifest("declaration-target", &[], &[]);
    manifest.provider_declarations =
        Some(serde_json::from_value(json!({"provider": {"version": 1}})).unwrap());
    hello(&mut target, manifest).await;
    let record = spawn.authority.capture().unwrap();
    let empty_digest = "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
    assert!(record.approved.entries.is_empty());
    assert_eq!(record.approved.digest, empty_digest);
    let entry = catalog_entry(&rig, "declaration-target").await;
    assert_eq!(entry["approved_declarations"], json!({}));
    assert_eq!(
        entry["requested_declarations"],
        json!({"provider": {"version": 1}})
    );
    assert_eq!(
        entry["approved_declarations_digest"],
        record.approved.digest
    );
    assert_eq!(entry["incarnation"], record.incarnation);
    assert_eq!(entry["spawn"], record.key.spawn);
    assert_eq!(entry["first_party"], false);
    let mut opener = Net::new(rig.router.clone()).await;
    for busy_refusals in [false, true] {
        let (_, reply) = open(
            &mut opener,
            &mut target,
            "declaration-target",
            busy_refusals,
        )
        .await;
        assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
        let target = &body(&reply)["target"];
        assert_eq!(
            target["approved_declarations_digest"],
            entry["approved_declarations_digest"]
        );
        assert_eq!(target["spawn"], entry["spawn"]);
        assert_eq!(target["incarnation"], entry["incarnation"]);
        assert_eq!(target["first_party"], false);
    }
}

#[tokio::test]
async fn relay_seams_authority_change_preserves_each_openers_close_reasons() {
    let mut rig = rig().await;
    let (mut target, spawn, _module) = supervised_target(&rig, "close-target", &[]).await;
    let mut modern = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    let mut legacy = claimed(&mut rig, ClaimPurpose::Helper).await;
    for (opener, accepts) in [(&mut modern, true), (&mut legacy, false)] {
        let frame = open_frame("close-target", false, None);
        let mut value = body(&frame);
        subc_control::RouteOpenRequestExt::new()
            .with_accepts_close_reasons(accepts.then(|| vec!["authority_changed".into()]))
            .write_to(&mut value)
            .unwrap();
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            OPEN_CORR,
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        opener.send(frame).await;
        let bind = next_bind(&mut target).await;
        ack(&mut target, &bind).await;
        let reply = open_reply(opener).await;
        assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    }
    rig.handler.change_spawn_authority(&spawn.authority);
    assert!(
        spawn.authority.alive(),
        "a live authority change, not an exit"
    );
    for (opener, expected) in [
        (&mut modern, RouteCloseReason::AuthorityChanged),
        (&mut legacy, RouteCloseReason::Reload),
    ] {
        let closed = loop {
            let frame = opener.recv().await.expect("route.closed push");
            if frame.header.ty == FrameType::Push && body(&frame)["op"] == "route.closed" {
                break body(&frame);
            }
        };
        let reason: RouteCloseReason = serde_json::from_value(closed["reason"].clone()).unwrap();
        assert_eq!(reason, expected, "{closed}");
        assert!(reason.may_reopen());
        assert_eq!(closed["terminal"], false);
        assert!(rig
            .handler
            .forwarding
            .route_authority_for_test(connection_of(&rig, opener))
            .is_empty());
    }
}

#[tokio::test]
async fn caller_attestation_claim_bound_openers_attest_their_spawn_and_claim() {
    let mut rig = rig().await;
    let mut target =
        unsupervised_target(&rig, "attest-target", &[CALLER_ATTESTATION_CAPABILITY]).await;
    let spawn = rig.spawn.authority.spawn();
    for (purpose, claim) in [
        (ClaimPurpose::SelfConnection, "own"),
        (ClaimPurpose::Helper, "brokered"),
        (ClaimPurpose::Relay, "relay"),
    ] {
        let mut opener = claimed(&mut rig, purpose).await;
        let (bind, reply) = open(&mut opener, &mut target, "attest-target", false).await;
        assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
        assert_eq!(
            bind["principal"],
            json!({"kind":"reserved","module_id":"claims-module"}),
            "{bind}"
        );
        let caller = &bind["caller"];
        assert_eq!(caller["claim"], claim, "{bind}");
        assert_eq!(caller["spawn"], spawn, "{bind}");
        assert_eq!(caller["authority_epoch"], 0, "{bind}");
        assert_eq!(
            caller["incarnation"],
            rig.handler.supervisor.daemon_incarnation()
        );
        assert_eq!(caller["origin"], "other");
        assert_eq!(
            caller["approved_declarations_digest"],
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        // No spawn is first-party yet: the daemon never raises the mark.
        assert_eq!(caller["first_party"], false, "{bind}");
        assert!(caller.get("declaration").is_none(), "{bind}");
        assert!(caller.get("declaration_digest").is_none(), "{bind}");
    }
}

#[tokio::test]
async fn caller_attestation_nonce_consumer_attests_legacy() {
    let rig = rig().await;
    let mut target =
        unsupervised_target(&rig, "legacy-target", &[CALLER_ATTESTATION_CAPABILITY]).await;
    let nonce = rig
        .handler
        .supervisor
        .spawn_nonce("claims-module")
        .expect("an ungated launch receives a nonce");
    let mut opener = Net::new(rig.router.clone()).await;
    opener
        .send(open_frame(
            "legacy-target",
            false,
            Some(subc_control::ConsumerIdentity {
                module_id: "claims-module".into(),
                launch_nonce: nonce,
            }),
        ))
        .await;
    let bind = next_bind(&mut target).await;
    let caller = &body(&bind)["caller"];
    assert_eq!(caller["claim"], "legacy", "{}", text(&bind));
    assert_eq!(caller["spawn"], rig.spawn.authority.spawn());
    assert_eq!(caller["first_party"], false);
}

#[tokio::test]
async fn caller_attestation_is_absent_for_direct_callers_and_undeclared_targets() {
    let mut rig = rig().await;
    let mut declared =
        unsupervised_target(&rig, "declared-target", &[CALLER_ATTESTATION_CAPABILITY]).await;
    let mut undeclared = unsupervised_target(&rig, "undeclared-target", &[]).await;
    let mut direct = Net::new(rig.router.clone()).await;
    let (bind, _) = open(&mut direct, &mut declared, "declared-target", false).await;
    assert!(bind.get("caller").is_none(), "a direct caller: {bind}");
    let mut own = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    let (bind, _) = open(&mut own, &mut undeclared, "undeclared-target", false).await;
    assert!(bind.get("caller").is_none(), "an undeclared target: {bind}");
}

#[tokio::test]
async fn caller_attestation_rides_the_modern_and_the_legacy_bind_encoder() {
    let mut rig = rig().await;
    let mut modern = unsupervised_target(
        &rig,
        "modern-target",
        &[
            CALLER_ATTESTATION_CAPABILITY,
            subc_protocol::scope::BIND_NO_PROJECT_CAPABILITY,
        ],
    )
    .await;
    let mut legacy = unsupervised_target(
        &rig,
        "legacy-encoder-target",
        &[CALLER_ATTESTATION_CAPABILITY],
    )
    .await;
    let mut own = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    let (bind, _) = open(&mut own, &mut modern, "modern-target", false).await;
    assert!(bind["identity"].get("project_root").is_none(), "{bind}");
    assert_eq!(bind["caller"]["claim"], "own", "{bind}");
    let (bind, _) = open(&mut own, &mut legacy, "legacy-encoder-target", false).await;
    assert!(bind["identity"].get("project_root").is_some(), "{bind}");
    assert_eq!(bind["caller"]["claim"], "own", "{bind}");
}

#[tokio::test]
async fn caller_attestation_reply_target_reports_the_reserved_endpoint() {
    let rig = rig().await;
    let mut unsupervised = unsupervised_target(&rig, "plain-target", &[]).await;
    let (mut supervised, spawn, _module) = supervised_target(&rig, "spawn-target", &[]).await;
    let mut opener = Net::new(rig.router.clone()).await;
    let incarnation = rig.handler.supervisor.daemon_incarnation();
    // Both reply constructions: the one prebuilt at reservation, and the one
    // rebuilt at commit for an opener that asked for busy refusals.
    for busy_refusals in [false, true] {
        let (_, reply) = open(
            &mut opener,
            &mut unsupervised,
            "plain-target",
            busy_refusals,
        )
        .await;
        let target = &body(&reply)["target"];
        assert_eq!(
            target,
            &json!({"incarnation": incarnation, "kind": "unsupervised", "first_party": false}),
            "busy_refusals {busy_refusals}: {}",
            text(&reply)
        );
        let (_, reply) = open(&mut opener, &mut supervised, "spawn-target", busy_refusals).await;
        let target = &body(&reply)["target"];
        assert_eq!(
            target,
            &json!({
                "incarnation": incarnation,
                "kind": "spawn",
                "spawn": spawn.authority.spawn(),
                "authority_epoch": 0,
                "first_party": false,
                "approved_declarations_digest":
                    "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
            }),
            "busy_refusals {busy_refusals}: {}",
            text(&reply)
        );
    }
}

#[tokio::test]
async fn caller_attestation_reply_target_never_reports_a_prefix_childs_owner() {
    let mut rig = rig().await;
    let mut child = super::claims::tests::prefix_connection(&mut rig, "claims:worker").await;
    // The prefix child registers over a claim bound to its owner's spawn,
    // but it is never that spawn's own registration.
    hello(&mut child, tool_manifest("claims:worker", &[], &[])).await;
    let mut opener = Net::new(rig.router.clone()).await;
    for busy_refusals in [false, true] {
        let (_, reply) = open(&mut opener, &mut child, "claims:worker", busy_refusals).await;
        assert_eq!(
            body(&reply)["target"],
            json!({
                "incarnation": rig.handler.supervisor.daemon_incarnation(),
                "kind": "prefix",
                "first_party": false,
            }),
            "{}",
            text(&reply)
        );
    }
}

#[tokio::test]
async fn caller_attestation_flow_scope_attests_relay_for_a_self_claim_carrier() {
    use subc_protocol::scope::{ScopeKind, ScopeRecord, ScopeSelector, FLOW_SCOPES_CAPABILITY};
    let mut rig = super::claims::tests::rig_with(|handler| {
        handler.with_scope_authority_owners(vec!["claims-module".into()])
    })
    .await;
    let mut target = unsupervised_target(
        &rig,
        "flow-target",
        &[CALLER_ATTESTATION_CAPABILITY, FLOW_SCOPES_CAPABILITY],
    )
    .await;
    // The carrier owns a flow scope and opens a route under it on its own
    // `self` connection: the route acts for the flow's code, not the carrier.
    let mut record = ScopeRecord::new("flow-scope", 1, ScopeKind::Head);
    record.attributes.flow_id = Some("flow:7".into());
    let synced = rig
        .primary
        .call(
            &rig.router,
            request(ModuleControlRequestFromModule::ScopeSync {
                generation: 1,
                scopes: vec![record],
            }),
        )
        .await;
    assert_eq!(synced.header.ty, FrameType::Response, "{}", text(&synced));
    let mut own = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    own.send(open_frame_scoped(
        "flow-target",
        false,
        None,
        Some(ScopeSelector::new(
            Principal::Reserved {
                module_id: "claims-module".into(),
            },
            "flow-scope".to_string(),
            Some(1),
        )),
    ))
    .await;
    if let Ok(reply) = tokio::time::timeout(Duration::from_millis(300), open_reply(&mut own)).await
    {
        panic!(
            "the open was answered before any bind: {} (sync: {})",
            text(&reply),
            text(&synced)
        );
    }
    let bind = body(&next_bind(&mut target).await);
    assert!(bind.to_string().contains("flow:7"), "{bind}");
    let caller = &bind["caller"];
    assert_eq!(caller["claim"], "relay", "{bind}");
    assert_eq!(caller["first_party"], false, "{bind}");
    assert!(caller.get("declaration").is_none(), "{bind}");
}

#[tokio::test]
async fn caller_attestation_stale_caller_epoch_settles_as_a_refusal() {
    let mut rig = rig().await;
    let mut target = unsupervised_target(
        &rig,
        "stale-caller-target",
        &[CALLER_ATTESTATION_CAPABILITY],
    )
    .await;
    let mut own = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    own.send(open_frame("stale-caller-target", false, None))
        .await;
    let bind = next_bind(&mut target).await;
    assert_eq!(body(&bind)["caller"]["authority_epoch"], 0);
    // The caller's authority changes after it was captured for this bind.
    rig.handler.change_spawn_authority(&rig.spawn.authority);
    ack(&mut target, &bind).await;
    let reply = open_reply(&mut own).await;
    assert_eq!(reply.header.ty, FrameType::Error, "{}", text(&reply));
    assert_eq!(
        body(&reply)["code"],
        subc_protocol::error_codes::STALE_AUTHORITY_EPOCH
    );
    // The shared module connection survives, and a new open binds at the
    // new epoch.
    let (bind, reply) = open(&mut own, &mut target, "stale-caller-target", false).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    assert_eq!(bind["caller"]["authority_epoch"], 1, "{bind}");
}

#[tokio::test]
async fn caller_attestation_stale_target_epoch_settles_without_closing_the_module() {
    let rig = rig().await;
    let (mut target, spawn, _module) = supervised_target(&rig, "stale-target", &[]).await;
    let mut opener = Net::new(rig.router.clone()).await;
    opener.send(open_frame("stale-target", false, None)).await;
    let bind = next_bind(&mut target).await;
    rig.handler.change_spawn_authority(&spawn.authority);
    ack(&mut target, &bind).await;
    let reply = open_reply(&mut opener).await;
    assert_eq!(reply.header.ty, FrameType::Error, "{}", text(&reply));
    assert_eq!(
        body(&reply)["code"],
        subc_protocol::error_codes::STALE_AUTHORITY_EPOCH
    );
    let (_, reply) = open(&mut opener, &mut target, "stale-target", false).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    assert_eq!(body(&reply)["target"]["authority_epoch"], 1);
}

#[tokio::test]
async fn caller_attestation_authority_change_closes_unscoped_routes_of_the_spawn() {
    let mut rig = rig().await;
    let (mut target, target_spawn, _module) = supervised_target(&rig, "change-target", &[]).await;
    let mut own = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    let (_, reply) = open(&mut own, &mut target, "change-target", false).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    // The route recorded both its caller's and its target's spawn at the
    // epochs it was attested at.
    let routes = rig
        .handler
        .forwarding
        .route_authority_for_test(connection_of(&rig, &own));
    assert_eq!(routes.len(), 1);
    let (caller, target_pair) = routes[0].clone();
    assert_eq!(caller.unwrap().key, rig.spawn.authority.key());
    assert_eq!(target_pair.unwrap().key, target_spawn.authority.key());

    rig.handler.change_spawn_authority(&rig.spawn.authority);
    let closed = loop {
        let frame = own.recv().await.expect("route.closed push");
        if frame.header.ty == FrameType::Push && body(&frame)["op"] == "route.closed" {
            break body(&frame);
        }
    };
    let reason: RouteCloseReason = serde_json::from_value(closed["reason"].clone()).unwrap();
    // The reason is `authority_changed`, or another reason that permits
    // reopening for an opener that did not declare it accepts that one.
    assert!(
        reason.may_reopen(),
        "a live spawn's authority change permits reopening: {closed}"
    );
    loop {
        let frame = target.recv().await.expect("module GOODBYE");
        if frame.header.ty == FrameType::Goodbye && frame.header.channel != 0 {
            break;
        }
    }
    assert!(rig
        .handler
        .forwarding
        .route_authority_for_test(connection_of(&rig, &own))
        .is_empty());
}

#[tokio::test]
async fn caller_attestation_registration_end_is_an_authority_change() {
    let rig = rig().await;
    let (target, spawn, _module) = supervised_target(&rig, "ending-target", &[]).await;
    let key = spawn.authority.key();
    assert_eq!(rig.handler.forwarding.published_spawn_epoch(&key), 0);
    drop(target);
    tokio::time::timeout(Duration::from_secs(5), async {
        while rig.handler.forwarding.published_spawn_epoch(&key) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the ended registration must publish a new epoch");
}

#[tokio::test]
async fn caller_attestation_deny_edges_read_the_opening_spawns_own_registration() {
    let rig = rig().await;
    let target = unsupervised_target(&rig, "guarded-target", &["guarded-cap/v1"]).await;
    let mut swap = swap_candidate(&rig, "deny-module").await;
    // Only the warming candidate denies the capability; the active
    // registration under the same id does not.
    let mut capabilities =
        json!({"provides": [], "requires": [], "must_never_reach": ["guarded-cap/v1"]});
    let update = swap
        .candidate
        .call(request(json!({
            "op": "catalog.update",
            "provides": [],
            "capabilities": capabilities.take(),
            "ready": false,
        })))
        .await;
    assert_eq!(update.header.ty, FrameType::Response, "{}", text(&update));
    let claim = super::claims::tests::net_mint(&mut swap.candidate, ClaimPurpose::SelfConnection)
        .await
        .claim;
    let mut opener = Net::new(rig.router.clone()).await;
    let present = opener.call(request(ClaimPresentRequest::new(claim))).await;
    assert_eq!(present.header.ty, FrameType::Response, "{}", text(&present));
    opener.send(open_frame("guarded-target", false, None)).await;
    let reply = open_reply(&mut opener).await;
    assert_eq!(reply.header.ty, FrameType::Error, "{}", text(&reply));
    assert_eq!(body(&reply)["code"], "capability_forbidden");
    drop(target);
}

#[tokio::test]
async fn spawn_authority_record_dies_at_the_child_exit_observation() {
    let rig = rig().await;
    let (_net, spawn, module, _) = network_primary(&rig, "exiting-module").await;
    assert!(spawn.authority.alive());
    let pid = module.status().unwrap().pid.expect("the child is running");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    tokio::time::timeout(Duration::from_secs(5), async {
        while spawn.authority.alive() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the exit observation must mark the record dead");
    assert!(
        spawn.authority.capture().is_none(),
        "a dead record attests nothing"
    );
    assert!(spawn.authority.epoch_for_test() > 0);
}

#[tokio::test]
async fn caller_attestation_capability_is_advertised_and_fixed_at_hello() {
    let rig = rig().await;
    let mut declared = Net::new(rig.router.clone()).await;
    let ack = hello(
        &mut declared,
        tool_manifest("frozen-declared", &[CALLER_ATTESTATION_CAPABILITY], &[]),
    )
    .await;
    assert!(
        text(&ack).contains(CALLER_ATTESTATION_CAPABILITY),
        "HELLO_ACK must advertise the capability: {}",
        text(&ack)
    );
    let mut undeclared = unsupervised_target(&rig, "frozen-undeclared", &[]).await;
    let role = serde_json::to_value(&tool_manifest("role", &[], &[]).provides).unwrap();
    for (net, provides) in [
        (&mut declared, json!([])),
        (&mut undeclared, json!([CALLER_ATTESTATION_CAPABILITY])),
    ] {
        let reply = net
            .call(request(json!({
                "op": "catalog.update",
                "provides": role.clone(),
                "capabilities": {"provides": provides, "requires": [], "must_never_reach": []},
            })))
            .await;
        assert_eq!(reply.header.ty, FrameType::Error, "{}", text(&reply));
        assert_eq!(
            body(&reply)["code"],
            error_codes::CATALOG_UPDATE_FROZEN_CAPABILITY
        );
    }
}

#[tokio::test]
async fn spawn_authority_promoted_candidate_keeps_its_launch_generation() {
    let rig = rig().await;
    let mut swap = swap_candidate(&rig, "promoted-module").await;
    let candidate = rig
        .handler
        .supervisor
        .registration_for_test("promoted-module")
        .unwrap();
    assert!(candidate.is_candidate());
    let generation = candidate.authority.spawn();
    let incarnation = candidate.authority.capture().unwrap().incarnation;
    let incumbent = catalog_entry(&rig, "promoted-module").await;
    assert_ne!(incumbent["spawn"], generation);
    let candidate_registration = rig
        .handler
        .registry
        .get_module_by_connection(swap.candidate_id)
        .unwrap()
        .unwrap();
    let candidate_entry = rig
        .handler
        .catalog_discovery_entry(&candidate_registration, true)
        .unwrap();
    assert_eq!(candidate_entry.spawn, Some(generation));
    assert_eq!(
        candidate_entry.incarnation.as_deref(),
        Some(incarnation.as_str())
    );
    let ready = swap
        .candidate
        .call(request(
            json!({"op":"catalog.update","provides":[],"ready":true}),
        ))
        .await;
    assert_eq!(ready.header.ty, FrameType::Response, "{}", text(&ready));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), &mut swap.swap)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        crate::supervise::SwapOutcome::Cutover
    );
    assert_eq!(swap.module.status().unwrap().spawn_generation, generation);
    let promoted = catalog_entry(&rig, "promoted-module").await;
    assert_eq!(promoted["incarnation"], incarnation);
    assert_eq!(promoted["spawn"], generation);
    assert_eq!(promoted["first_party"], false);
    assert_eq!(
        promoted["approved_declarations_digest"],
        candidate_entry.approved_declarations_digest.unwrap()
    );
}

#[test]
fn caller_attestation_federated_and_flow_scopes_attest_relay() {
    use subc_protocol::scope::{ScopeKind, ScopeStamp};
    let federated = ScopeStamp::new(Principal::Direct, "remote", 1, ScopeKind::Federated);
    let head = ScopeStamp::new(Principal::Direct, "head", 1, ScopeKind::Head);
    let mut flow = head.clone();
    flow.attributes.flow_id = Some("flow".into());
    assert!(caller_attestation::scope_acts_for_another(&federated));
    assert!(caller_attestation::scope_acts_for_another(&flow));
    assert!(!caller_attestation::scope_acts_for_another(&head));

    let record = crate::spawn_authority::SpawnAuthorityRecord::new("incarnation", 4, "carrier");
    let own = crate::spawn_authority::CallerCapture {
        authority: record.capture().unwrap(),
        claim: subc_protocol::peer::CallerClaim::Own,
    };
    let relayed = own.relayed().attestation("target").unwrap();
    assert_eq!(relayed.claim, subc_protocol::peer::CallerClaim::Relay);
    assert!(!relayed.first_party);
}
