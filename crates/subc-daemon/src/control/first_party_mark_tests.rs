//! What the first-party mark's activation and lowering publish, driven
//! through real server connections: the epoch fence, route closes, the
//! catalog, and the `caller` and `target` attestations. The gate's decision
//! of when to activate or lower is tested beside the gate.

use super::*;
use crate::config_gate::runtime::mark::MarkPublisher;
use crate::control::caller_attestation::ControlMarkPublisher;
use subc_protocol::peer::CallerOrigin;

fn publisher(rig: &Rig) -> ControlMarkPublisher {
    ControlMarkPublisher((*rig.handler).clone())
}

/// Open a route, declaring whether the opener decodes `authority_changed`.
async fn open_declaring(
    opener: &mut Net,
    target: &mut Net,
    module_id: &str,
    accepts: bool,
) -> Frame {
    let mut value = body(&open_frame(module_id, false, None));
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
    let bind = next_bind(target).await;
    ack(target, &bind).await;
    let reply = open_reply(opener).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    reply
}

async fn route_closed(opener: &mut Net) -> Value {
    loop {
        let frame = opener.recv().await.expect("route.closed push");
        if frame.header.ty == FrameType::Push && body(&frame)["op"] == "route.closed" {
            return body(&frame);
        }
    }
}

#[tokio::test]
async fn first_party_mark_activation_keeps_epochs_and_routes_and_a_pending_bind_commits_unmarked() {
    let mut rig = rig().await;
    rig.spawn
        .authority
        .set_origin_for_test(CallerOrigin::Release);
    let (mut target, target_spawn, _module) =
        supervised_target(&rig, "mark-target", &[CALLER_ATTESTATION_CAPABILITY]).await;
    target_spawn
        .authority
        .set_origin_for_test(CallerOrigin::Release);
    let mut own = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    let (early, reply) = open(&mut own, &mut target, "mark-target", false).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    assert_eq!(early["caller"]["first_party"], false, "{early}");
    // A second open is attested now and is still waiting at the target when
    // both spawns are activated.
    own.send(open_frame("mark-target", false, None)).await;
    let pending = next_bind(&mut target).await;
    let mut catalog = rig.handler.registry.subscribe_catalog_changes().unwrap();
    catalog.borrow_and_update();

    let publisher = publisher(&rig);
    assert!(publisher.activate(&rig.spawn.authority));
    assert!(publisher.activate(&target_spawn.authority));
    for key in [rig.spawn.authority.key(), target_spawn.authority.key()] {
        assert_eq!(
            rig.handler.forwarding.published_spawn_epoch(&key),
            crate::spawn_authority::INITIAL_AUTHORITY_EPOCH,
            "activation does not bump the epoch"
        );
    }
    tokio::time::timeout(Duration::from_secs(5), catalog.changed())
        .await
        .expect("activation advances the catalog")
        .unwrap();

    // The pending bind commits with the `first_party: false` it was attested
    // with, and the earlier route is still bound.
    ack(&mut target, &pending).await;
    let reply = open_reply(&mut own).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    assert_eq!(body(&pending)["caller"]["first_party"], false);
    assert_eq!(body(&reply)["target"]["first_party"], false);
    assert_eq!(
        rig.handler
            .forwarding
            .route_authority_for_test(connection_of(&rig, &own))
            .len(),
        2,
        "activation closes no route"
    );

    // Routes opened from now on carry the mark on both sides.
    let (bind, reply) = open(&mut own, &mut target, "mark-target", false).await;
    assert_eq!(bind["caller"]["first_party"], true, "{bind}");
    assert_eq!(bind["caller"]["claim"], "own");
    assert_eq!(bind["caller"]["authority_epoch"], 0);
    let target_facts = &body(&reply)["target"];
    assert_eq!(target_facts["first_party"], true, "{target_facts}");
    assert_eq!(target_facts["authority_epoch"], 0);
    assert_eq!(
        catalog_entry(&rig, "mark-target").await["first_party"],
        true
    );
}

#[tokio::test]
async fn first_party_mark_lowering_bumps_the_epoch_and_closes_routes_per_recipient() {
    let mut rig = rig().await;
    let (mut target, spawn, _module) = supervised_target(&rig, "lowered-target", &[]).await;
    spawn.authority.set_origin_for_test(CallerOrigin::Release);
    let publisher = publisher(&rig);
    assert!(publisher.activate(&spawn.authority));
    let mut modern = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    let mut legacy = claimed(&mut rig, ClaimPurpose::Helper).await;
    for (opener, accepts) in [(&mut modern, true), (&mut legacy, false)] {
        let reply = open_declaring(opener, &mut target, "lowered-target", accepts).await;
        assert_eq!(body(&reply)["target"]["first_party"], true);
    }

    publisher.lower(&spawn.authority);
    assert_eq!(
        rig.handler
            .forwarding
            .published_spawn_epoch(&spawn.authority.key()),
        1,
        "lowering bumps the epoch"
    );
    for (opener, expected) in [
        (&mut modern, RouteCloseReason::AuthorityChanged),
        (&mut legacy, RouteCloseReason::Reload),
    ] {
        let closed = route_closed(opener).await;
        let reason: RouteCloseReason = serde_json::from_value(closed["reason"].clone()).unwrap();
        assert_eq!(reason, expected, "{closed}");
        assert!(reason.may_reopen());
    }
    // Reopening binds at the new epoch, unmarked.
    let (_, reply) = open(&mut modern, &mut target, "lowered-target", false).await;
    assert_eq!(reply.header.ty, FrameType::Response, "{}", text(&reply));
    assert_eq!(body(&reply)["target"]["first_party"], false);
    assert_eq!(body(&reply)["target"]["authority_epoch"], 1);
    assert_eq!(
        catalog_entry(&rig, "lowered-target").await["first_party"],
        false
    );
}

#[tokio::test]
async fn first_party_mark_development_spawn_carries_the_entries_approved_for_it() {
    let mut rig = rig().await;
    let record = rig.spawn.authority.clone();
    record.set_origin_for_test(CallerOrigin::Development);
    record.set_development_daemon_for_test(true);
    let entry: serde_json::Map<String, Value> =
        serde_json::from_value(json!({"grants": ["read"]})).unwrap();
    record.set_approved_for_test(std::collections::BTreeMap::from([(
        "grant-provider".to_owned(),
        entry.clone(),
    )]));
    let mut provider =
        unsupervised_target(&rig, "grant-provider", &[CALLER_ATTESTATION_CAPABILITY]).await;
    let mut other =
        unsupervised_target(&rig, "other-provider", &[CALLER_ATTESTATION_CAPABILITY]).await;
    let mut own = claimed(&mut rig, ClaimPurpose::SelfConnection).await;
    let mut helper = claimed(&mut rig, ClaimPurpose::Helper).await;
    assert!(publisher(&rig).activate(&record));

    let (bind, _) = open(&mut own, &mut provider, "grant-provider", false).await;
    let caller = &bind["caller"];
    assert_eq!(caller["first_party"], true, "{bind}");
    assert_eq!(caller["origin"], "development");
    assert_eq!(caller["declaration"], Value::Object(entry.clone()));
    assert_eq!(
        caller["declaration_digest"],
        crate::spawn_authority::declaration_digest("grant-provider", &entry).unwrap()
    );
    assert_eq!(
        caller["approved_declarations_digest"],
        record.approved().digest
    );

    // No entry is approved for this provider, so none is sent.
    let (bind, _) = open(&mut own, &mut other, "other-provider", false).await;
    assert_eq!(bind["caller"]["first_party"], true, "{bind}");
    assert!(bind["caller"].get("declaration").is_none(), "{bind}");
    assert!(bind["caller"].get("declaration_digest").is_none(), "{bind}");

    // A helper of the same spawn is never first-party and carries no entry.
    let (bind, _) = open(&mut helper, &mut provider, "grant-provider", false).await;
    assert_eq!(bind["caller"]["first_party"], false, "{bind}");
    assert!(bind["caller"].get("declaration").is_none(), "{bind}");
}
