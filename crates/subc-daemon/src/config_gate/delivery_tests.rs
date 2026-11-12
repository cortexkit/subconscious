use super::*;
use crate::config_gate::runtime::{AppliedDisposition, DeliveryObserver};
use subc_protocol::config::{ConfigAppliedRequest, ConfigApplyResult};

const INCUMBENT: ConnectionId = ConnectionId::new(10);
const CANDIDATE: ConnectionId = ConnectionId::new(20);

fn rig() -> (Rig, RuntimeOwner) {
    let rig = Rig::new();
    write(&rig, 1);
    let owner = rig.owner("delivery");
    rig.declare(&owner, INCUMBENT.get());
    rig.stamp(&owner);
    (rig, owner)
}

fn write(rig: &Rig, limit: u64) {
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        serde_json::to_vec(&json!({
            "credential":{"vault_ref":"approved-reference"},"limit":limit,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn pending(owner: &RuntimeOwner, connection: ConnectionId) -> ConfigView {
    let reply = owner
        .module_get(connection, "a", ConfigGetRequest::new())
        .unwrap();
    let active = owner.lock().unwrap().active_declarations.get("a") == Some(&connection);
    owner
        .module_view_delivered(connection, "a", &reply, tokio::time::Instant::now(), active)
        .unwrap();
    reply.pending.unwrap()
}

fn applied(
    owner: &RuntimeOwner,
    connection: ConnectionId,
    view: &ConfigView,
    ack: bool,
) -> Result<AppliedDisposition> {
    owner.module_applied(
        connection,
        "a",
        ConfigAppliedRequest::new(
            view.generation,
            view.attempt.unwrap(),
            if ack {
                ConfigApplyResult::Ack
            } else {
                ConfigApplyResult::Nack
            },
            vec![],
        ),
    )
}

fn approve(rig: &Rig, owner: &RuntimeOwner, limit: u64) -> u64 {
    write(rig, limit);
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            limit,
        )
        .unwrap()
}

fn candidate(rig: &Rig, owner: &RuntimeOwner) {
    owner
        .declare(CANDIDATE, "a", rig.declaration.clone(), false)
        .unwrap();
}

#[test]
fn config_delivery_first_ack_activates_whole_generation_and_duplicates_do_nothing() {
    let (_rig, owner) = rig();
    let view = pending(&owner, INCUMBENT);
    assert_eq!(view.generation, 1);
    let before = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    assert!(before.active.is_none());
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::Active
    );
    assert_eq!(
        applied(&owner, INCUMBENT, &view, false).unwrap(),
        AppliedDisposition::Ignored
    );
    let reply = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(reply.active.unwrap().generation, 1);
    assert!(reply.pending.is_none());
}

#[test]
fn config_delivery_get_binds_exact_generation_and_attempt_to_one_child() {
    let (rig, owner) = rig();
    let view = pending(&owner, INCUMBENT);
    let mut request = ConfigGetRequest::new();
    request.generation = Some(view.generation);
    request.attempt = Some(view.attempt.unwrap() + 1);
    assert_eq!(
        owner
            .module_get(INCUMBENT, "a", request.clone())
            .unwrap_err()
            .code,
        "config_gated"
    );
    request.attempt = view.attempt;
    assert_eq!(
        owner
            .module_get(INCUMBENT, "a", request.clone())
            .unwrap()
            .pending
            .unwrap()
            .attempt,
        view.attempt
    );
    candidate(&rig, &owner);
    assert_eq!(
        owner.module_get(CANDIDATE, "a", request).unwrap_err().code,
        "config_gated"
    );
    assert_eq!(
        applied(&owner, CANDIDATE, &view, true).unwrap_err().code,
        "not_delivery_target"
    );
    assert!(owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap()
        .active
        .is_none());
}

#[test]
fn config_delivery_newest_approval_supersedes_unacked_generation_across_restart() {
    let (rig, owner) = rig();
    let old = pending(&owner, INCUMBENT);
    assert_eq!(approve(&rig, &owner, 2), 2);
    assert_eq!(
        applied(&owner, INCUMBENT, &old, false).unwrap(),
        AppliedDisposition::Ignored
    );
    assert_eq!(
        applied(&owner, INCUMBENT, &old, true).unwrap(),
        AppliedDisposition::Ignored
    );
    let newest = pending(&owner, INCUMBENT);
    assert_eq!(newest.generation, 2);
    assert_eq!(newest.effective["limit"], 2);
    drop(owner);
    let restarted = rig.owner("delivery-restarted");
    rig.declare(&restarted, 30);
    let resumed = pending(&restarted, ConnectionId::new(30));
    assert_eq!(resumed.generation, 2);
    assert_eq!(resumed.effective["limit"], 2);
}

#[test]
fn config_delivery_nack_restores_active_and_durable_mark_refuses_failed_and_edited_bytes() {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve(&rig, &owner, 2);
    let failed = pending(&owner, INCUMBENT);
    assert_eq!(
        applied(&owner, INCUMBENT, &failed, false).unwrap(),
        AppliedDisposition::Failed {
            generation: 2,
            restore: Some(1),
            keep_running: false,
        }
    );
    write(&rig, 99);
    drop(owner);
    let restarted = rig.owner("after-nack");
    rig.declare(&restarted, 30);
    let reply = restarted
        .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
        .unwrap();
    assert!(reply.pending.is_none());
    let active = reply.active.unwrap();
    assert_eq!(active.generation, 1);
    assert_eq!(active.effective["limit"], 1);
    let mut state = restarted.lock().unwrap();
    assert_eq!(
        state.store.body().unwrap().generations[&2]
            .generation
            .never_redeliver,
        vec!["a"]
    );
}

#[test]
fn config_delivery_daemon_approval_preserves_failed_and_active_module_state() {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve(&rig, &owner, 2);
    let failed = pending(&owner, INCUMBENT);
    applied(&owner, INCUMBENT, &failed, false).unwrap();
    owner.take_admission_transitions().unwrap();
    let path = rig.path.join("subc.jsonc");
    let mut daemon: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    daemon["port"] = json!(4321);
    std::fs::write(path, serde_json::to_vec(&daemon).unwrap()).unwrap();
    let view = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &view)
        .unwrap();
    let approval = owner.begin_approval(&view.digest, &Rig::facts()).unwrap();
    let generation = owner
        .finish_approval(
            approval.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            3,
        )
        .unwrap();
    assert_eq!(generation, 3);
    assert!(owner
        .authority_exit_configuration(generation)
        .unwrap()
        .is_some());
    owner.complete_authority_exit(generation).unwrap();
    let reply = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    assert!(
        reply.pending.is_none(),
        "a daemon-only approval must not redeliver failed module bytes"
    );
    assert_eq!(reply.active.unwrap().generation, 1);
    assert_eq!(
        owner.lock().unwrap().store.body().unwrap().generations[&2]
            .generation
            .never_redeliver,
        ["a"]
    );
    assert!(
        owner.lock().unwrap().admission_releases.is_empty(),
        "normal reconciliation must not revive unrelated module actors"
    );
}

fn active_catchup() -> (Rig, RuntimeOwner) {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    candidate(&rig, &owner);
    let served = owner
        .module_get(CANDIDATE, "a", ConfigGetRequest::new())
        .unwrap();
    owner
        .module_view_delivered(CANDIDATE, "a", &served, tokio::time::Instant::now(), false)
        .unwrap();
    assert_eq!(served.active.unwrap().generation, 1);
    assert!(served.pending.is_none());
    approve(&rig, &owner, 2);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    owner
        .promoted(CANDIDATE, "a", tokio::time::Instant::now())
        .unwrap()
        .unwrap();
    (rig, owner)
}

fn store_bytes(rig: &Rig) -> Vec<u8> {
    let path = rig.path.join("store").join(crate::config_store::STORE_FILE);
    std::fs::read(path).unwrap()
}

fn sealed_owner(
    rig: &Rig,
    key: &crate::config_store::TestStoreKey,
    incarnation: &str,
) -> RuntimeOwner {
    RuntimeOwner::new(
        incarnation.into(),
        ConfigStore::open_sealed_for_tests(
            StoreRoot::from_service_definition(rig.path.join("store")).unwrap(),
            key.clone(),
        ),
        ConfigCapturer {
            daemon_path: rig.path.join("subc.jsonc"),
            config_home: rig.path.clone(),
            projects: std::sync::Arc::new(super::super::super::capture::UnavailableProjectRegistry),
        },
    )
}

fn nack_while_anchor_pending() -> (Rig, crate::config_store::TestStoreKey, RuntimeOwner) {
    let rig = Rig::new();
    write(&rig, 1);
    let key = crate::config_store::TestStoreKey::default();
    let owner = sealed_owner(&rig, &key, "sealed-delivery");
    rig.declare(&owner, INCUMBENT.get());
    rig.stamp(&owner);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve(&rig, &owner, 2);
    let view = pending(&owner, INCUMBENT);
    key.fail_anchor(true);
    write(&rig, 3);
    assert_eq!(owner.status().unwrap_err().code, "anchor_pending");
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap_err().code,
        "anchor_pending"
    );
    assert_eq!(
        applied(&owner, INCUMBENT, &view, false).unwrap_err().code,
        "anchor_pending"
    );
    {
        let mut state = owner.lock().unwrap();
        let body = state.store.body().unwrap();
        assert_eq!(
            (body.versions[0].1.active, body.versions[0].1.pending),
            (Some(1), None)
        );
        assert_eq!(body.generations[&2].generation.never_redeliver, vec!["a"]);
    }
    (rig, key, owner)
}

#[tokio::test(start_paused = true)]
async fn config_delivery_anchor_retry_makes_nack_durable_and_is_idle_after_recovery() {
    let (rig, key, owner) = nack_while_anchor_pending();
    let calls = key.anchor_calls();
    let due = owner.retry_anchor().unwrap().unwrap();
    assert_eq!(key.anchor_calls(), calls);
    tokio::time::advance(due - tokio::time::Instant::now() - Duration::from_nanos(1)).await;
    assert_eq!(owner.retry_anchor().unwrap(), Some(due));
    assert_eq!(key.anchor_calls(), calls);
    key.fail_anchor(false);
    tokio::time::advance(Duration::from_nanos(1)).await;
    assert!(owner.retry_anchor().unwrap().is_none());
    assert_eq!(
        key.anchor_calls(),
        calls + 2,
        "anchor the committed snapshot, then the deferred failure"
    );
    tokio::time::advance(Duration::from_secs(600)).await;
    assert!(owner.retry_anchor().unwrap().is_none());
    assert_eq!(key.anchor_calls(), calls + 2);
    drop(owner);
    let restarted = sealed_owner(&rig, &key, "after-anchor-retry");
    rig.declare(&restarted, 30);
    let reply = restarted
        .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
        .unwrap();
    assert!(reply.pending.is_none());
    assert_eq!(reply.active.unwrap().generation, 1);
    let mut state = restarted.lock().unwrap();
    assert_eq!(
        state.store.body().unwrap().generations[&2]
            .generation
            .never_redeliver,
        vec!["a"]
    );
}

#[tokio::test(start_paused = true)]
async fn config_delivery_anchor_pending_restart_before_failure_anchor_may_redeliver_once() {
    let durable_body = |rig: &Rig| -> crate::config_store::StoreBody {
        let envelope: serde_json::Value = serde_json::from_slice(
            &std::fs::read(rig.path.join("store").join(crate::config_store::STORE_FILE)).unwrap(),
        )
        .unwrap();
        serde_json::from_value(envelope["body"].clone()).unwrap()
    };
    let (rig, key, owner) = nack_while_anchor_pending();
    // This read does not consult the owner's deferred in-memory failure. The
    // daemon dies while the durable bytes still permit generation 2.
    let before_crash = durable_body(&rig);
    assert_eq!(before_crash.versions[0].1.pending, Some(2));
    assert!(!before_crash.generations[&2]
        .generation
        .never_redeliver
        .contains(&"a".into()));
    drop(owner);
    let restarted = sealed_owner(&rig, &key, "before-anchor-retry");
    rig.declare(&restarted, 30);
    for _ in 0..3 {
        assert_eq!(
            restarted
                .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
                .unwrap_err()
                .code,
            "anchor_pending"
        );
        assert!(restarted
            .delivery_window(ConnectionId::new(30))
            .unwrap()
            .is_none());
    }
    key.fail_anchor(false);
    let due = restarted.retry_anchor().unwrap().unwrap();
    tokio::time::advance(due - tokio::time::Instant::now()).await;
    assert!(restarted.retry_anchor().unwrap().is_none());
    let reply = restarted
        .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(reply.active.unwrap().generation, 1);
    let view = pending(&restarted, ConnectionId::new(30));
    assert_eq!(
        view.generation, 2,
        "R5 permits one redelivery when the in-memory failure was never anchored"
    );
    let attempt = view
        .attempt
        .expect("the new connection owns a new bound attempt");
    for _ in 0..3 {
        assert_eq!(
            restarted
                .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
                .unwrap()
                .pending
                .unwrap()
                .attempt,
            Some(attempt)
        );
    }
    key.fail_anchor(true);
    assert_eq!(
        applied(&restarted, ConnectionId::new(30), &view, false)
            .unwrap_err()
            .code,
        "anchor_pending"
    );
    let unanchored_failure = durable_body(&rig);
    assert_eq!(unanchored_failure.versions[0].1.active, Some(1));
    assert_eq!(unanchored_failure.versions[0].1.pending, None);
    assert_eq!(
        restarted.delivery_take_recoveries().unwrap(),
        vec![(
            ConnectionId::new(30),
            "a".into(),
            AppliedDisposition::Failed {
                generation: 2,
                restore: Some(1),
                keep_running: false,
            },
        )]
    );
    assert_eq!(
        unanchored_failure.generations[&2]
            .generation
            .never_redeliver,
        vec!["a"]
    );
    for _ in 0..3 {
        assert_eq!(
            restarted
                .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
                .unwrap_err()
                .code,
            "anchor_pending"
        );
        assert!(restarted
            .delivery_window(ConnectionId::new(30))
            .unwrap()
            .is_none());
    }
    key.fail_anchor(false);
    if let Some(due) = restarted.retry_anchor().unwrap() {
        tokio::time::advance(due.saturating_duration_since(tokio::time::Instant::now())).await;
        assert!(restarted.retry_anchor().unwrap().is_none());
    }
    assert!(restarted
        .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
        .unwrap()
        .pending
        .is_none());
    drop(restarted);
    for connection in [40, 50] {
        let final_owner = sealed_owner(&rig, &key, &format!("after-durable-nack-{connection}"));
        rig.declare(&final_owner, connection);
        assert!(final_owner
            .module_get(ConnectionId::new(connection), "a", ConfigGetRequest::new())
            .unwrap()
            .pending
            .is_none());
        assert!(final_owner
            .delivery_window(ConnectionId::new(connection))
            .unwrap()
            .is_none());
    }
    let anchored_failure = durable_body(&rig);
    assert_eq!(anchored_failure.versions[0].1.active, Some(1));
    assert_eq!(
        anchored_failure.generations[&2].generation.never_redeliver,
        vec!["a"]
    );
    assert_eq!(anchored_failure.generations[&2].outcome, "failed");
}

#[tokio::test(start_paused = true)]
async fn config_delivery_ack_anchor_failure_stays_pending_on_status_until_recovery() {
    let rig = Rig::new();
    write(&rig, 1);
    let key = crate::config_store::TestStoreKey::default();
    let owner = sealed_owner(&rig, &key, "unanchored-ack");
    rig.declare(&owner, INCUMBENT.get());
    rig.stamp(&owner);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve(&rig, &owner, 2);
    let view = pending(&owner, INCUMBENT);
    key.fail_anchor(true);
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap_err().code,
        "anchor_pending"
    );
    let calls = key.anchor_calls();
    let status = owner.status().unwrap();
    assert_eq!(
        key.anchor_calls(),
        calls + 1,
        "operator status retries a pending anchor exactly once"
    );
    assert!(status.anchor_pending);
    assert_eq!(status.approved_but_inactive[0]["generation"], 2);
    let mut state = owner.lock().unwrap();
    assert_eq!(state.store.body().unwrap().versions[0].1.active, Some(1));
    drop(state);
    key.fail_anchor(false);
    assert!(!owner.status().unwrap().anchor_pending);
    let reply = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(reply.active.unwrap().generation, 2);
    assert!(reply.pending.is_none());
}

#[test]
fn config_delivery_swap_catchup_ack_clears_only_child_attempt_without_rotating_recovery() {
    let (rig, owner) = active_catchup();
    let bytes = store_bytes(&rig);
    let reply = owner
        .module_get(CANDIDATE, "a", ConfigGetRequest::new())
        .unwrap();
    owner
        .module_view_delivered(CANDIDATE, "a", &reply, tokio::time::Instant::now(), true)
        .unwrap();
    assert_eq!(reply.active.unwrap().generation, 2);
    let catchup = reply.pending.unwrap();
    assert_eq!(catchup.generation, 2);
    assert_eq!(
        applied(&owner, CANDIDATE, &catchup, true).unwrap(),
        AppliedDisposition::CatchupComplete
    );
    assert_eq!(store_bytes(&rig), bytes);
    assert!(owner
        .module_get(CANDIDATE, "a", ConfigGetRequest::new())
        .unwrap()
        .pending
        .is_none());
}

#[test]
fn config_delivery_swap_catchup_nack_does_not_poison_the_active_generation() {
    let (rig, owner) = active_catchup();
    let bytes = store_bytes(&rig);
    let catchup = pending(&owner, CANDIDATE);
    assert_eq!(
        applied(&owner, CANDIDATE, &catchup, false).unwrap(),
        AppliedDisposition::CatchupFailed { active: 2 }
    );
    assert_eq!(store_bytes(&rig), bytes);
    let mut state = owner.lock().unwrap();
    let body = state.store.body().unwrap();
    let versions = &body.versions[0].1;
    assert_eq!(
        (versions.active, versions.recovery, versions.pending),
        (Some(2), Some(1), None)
    );
    assert!(body.generations[&2].generation.never_redeliver.is_empty());
}

#[test]
fn config_delivery_swap_pending_at_cutover_uses_fresh_attempt_and_rejects_pre_cutover_ack() {
    let (rig, owner) = rig();
    candidate(&rig, &owner);
    let before = pending(&owner, CANDIDATE);
    assert_eq!(
        applied(&owner, CANDIDATE, &before, true).unwrap_err().code,
        "not_delivery_target"
    );
    let (_, attempt) = owner
        .promoted(CANDIDATE, "a", tokio::time::Instant::now())
        .unwrap()
        .unwrap();
    assert_ne!(Some(attempt), before.attempt);
    assert_eq!(
        applied(&owner, CANDIDATE, &before, true).unwrap(),
        AppliedDisposition::Ignored
    );
    assert_eq!(
        applied(&owner, CANDIDATE, &pending(&owner, CANDIDATE), true).unwrap(),
        AppliedDisposition::Active
    );
}

fn approve_kinds(
    rig: &Rig,
    owner: &RuntimeOwner,
    kinds: &[subc_config::ApplyKind],
) -> ConfigPlanView {
    let plan = plan_kinds(rig, owner, kinds).unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap();
    plan
}

fn plan_kinds(
    rig: &Rig,
    owner: &RuntimeOwner,
    kinds: &[subc_config::ApplyKind],
) -> super::super::super::Result<ConfigPlanView> {
    write(rig, 2);
    plan_kinds_from_disk(owner, kinds)
}

#[tokio::test]
async fn config_admission_route_rebind_ack_revokes_old_routes_and_allows_reopen() {
    let (rig, owner) = rig();
    let first = pending(&owner, INCUMBENT);
    applied(&owner, INCUMBENT, &first, true).unwrap();
    let owner = std::sync::Arc::new(owner);
    let forwarding = std::sync::Arc::new(crate::ForwardingTable::default());
    let (module_tx, _module_rx) = tokio::sync::mpsc::channel(32);
    forwarding
        .register_module_connection(
            INCUMBENT,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(module_tx),
        )
        .unwrap();
    let (client_tx, mut client_rx) = tokio::sync::mpsc::channel(32);
    let sink = crate::FrameSink::new(client_tx);
    let client = ConnectionId::new(30);
    let old = forwarding
        .begin_route_bind_relay_for_test(client, sink.clone(), 1, "a")
        .unwrap();
    forwarding
        .complete_pending_relay(
            INCUMBENT,
            old.corr,
            crate::forwarding::RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    let _handler = crate::ControlHandler::with_forwarding(
        std::sync::Arc::new(crate::Registry::default()),
        forwarding.clone(),
    )
    .with_config_runtime(owner.clone());
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    let view = pending(&owner, INCUMBENT);
    owner
        .install_approved_route_policy("a", view.generation)
        .await
        .unwrap();
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        1,
        "no old route closes before the module ACK"
    );
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::WaitingForReadiness
    );
    owner
        .install_approved_route_policy("a", view.generation)
        .await
        .unwrap();
    assert_eq!(forwarding.active_binding_count().unwrap(), 0);
    let mut saw_reload = false;
    while let Ok(frame) = client_rx.try_recv() {
        if frame.frame.header.ty == subc_protocol::FrameType::Push {
            let push: serde_json::Value = serde_json::from_slice(&frame.frame.body).unwrap();
            saw_reload |= push["reason"] == "reload";
        }
    }
    assert!(
        saw_reload,
        "clients need the existing reopen-eligible reload reason"
    );
    let new = forwarding
        .begin_route_bind_relay_for_test(client, sink, 2, "a")
        .unwrap();
    forwarding
        .complete_pending_relay(
            INCUMBENT,
            new.corr,
            crate::forwarding::RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);
    assert_eq!(
        owner
            .module_get(INCUMBENT, "a", ConfigGetRequest::new())
            .unwrap()
            .pending
            .unwrap()
            .effective["limit"],
        2
    );
}

fn acknowledged_route_fixture() -> (
    Rig,
    std::sync::Arc<RuntimeOwner>,
    std::sync::Arc<crate::ForwardingTable>,
    ConfigView,
) {
    let (rig, owner) = rig();
    let first = pending(&owner, INCUMBENT);
    applied(&owner, INCUMBENT, &first, true).unwrap();
    let owner = std::sync::Arc::new(owner);
    let forwarding = std::sync::Arc::new(crate::ForwardingTable::default());
    let (module_tx, _module_rx) = tokio::sync::mpsc::channel(32);
    forwarding
        .register_module_connection(
            INCUMBENT,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(module_tx),
        )
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    let route = forwarding
        .begin_route_bind_relay_for_test(ConnectionId::new(30), crate::FrameSink::new(tx), 1, "a")
        .unwrap();
    forwarding
        .complete_pending_relay(
            INCUMBENT,
            route.corr,
            crate::forwarding::RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    let _handler = crate::ControlHandler::with_forwarding(
        std::sync::Arc::new(crate::Registry::default()),
        forwarding.clone(),
    )
    .with_config_runtime(owner.clone());
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    let view = pending(&owner, INCUMBENT);
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::WaitingForReadiness
    );
    (rig, owner, forwarding, view)
}

#[tokio::test]
async fn config_admission_old_connection_ack_cannot_revoke_for_a_new_attempt() {
    let (rig, owner, forwarding, view) = acknowledged_route_fixture();
    rig.declare(&owner, 20);
    let current = pending(&owner, ConnectionId::new(20));
    assert_ne!(current.attempt, view.attempt);
    owner
        .install_approved_route_policy("a", view.generation)
        .await
        .unwrap();
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        1,
        "the incumbent's ACK cannot install policy for the new child"
    );
}

#[tokio::test(start_paused = true)]
async fn config_admission_ack_expired_before_revocation_closes_no_routes() {
    let (_rig, owner, forwarding, view) = acknowledged_route_fixture();
    tokio::time::advance(Duration::from_secs(30)).await;
    owner
        .install_approved_route_policy("a", view.generation)
        .await
        .unwrap();
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        1,
        "an ACK cannot authorize revocation after its delivery deadline"
    );
}

#[tokio::test(start_paused = true)]
async fn config_admission_route_rebind_rechecks_deadline_after_forwarding_write_wait() {
    let (_rig, owner, forwarding, view) = acknowledged_route_fixture();
    let locked = std::sync::Arc::new(std::sync::Barrier::new(2));
    let unlock = std::sync::Arc::new(std::sync::Barrier::new(2));
    let locker = {
        let (forwarding, locked, unlock) = (forwarding.clone(), locked.clone(), unlock.clone());
        std::thread::spawn(move || {
            forwarding.with_forwarding_write_lock_for_test(|| {
                locked.wait();
                unlock.wait();
            })
        })
    };
    locked.wait();
    let hold = std::sync::Arc::new(std::sync::Barrier::new(2));
    forwarding.pause_policy_rebind_before_write(hold.clone());
    let runtime = tokio::runtime::Handle::current();
    let installation = std::thread::spawn(move || {
        runtime.block_on(owner.install_approved_route_policy("a", view.generation))
    });
    hold.wait();
    tokio::time::advance(Duration::from_secs(30)).await;
    hold.wait();
    unlock.wait();
    locker.join().unwrap();
    let _ = installation.join().unwrap();
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        1,
        "a deadline crossed while awaiting forwarding publication must revoke no routes"
    );
}

#[tokio::test(start_paused = true)]
async fn config_admission_route_rebind_nack_and_timeout_close_nothing() {
    for timed_out in [false, true] {
        let (rig, owner) = rig();
        let first = pending(&owner, INCUMBENT);
        applied(&owner, INCUMBENT, &first, true).unwrap();
        let owner = std::sync::Arc::new(owner);
        let forwarding = std::sync::Arc::new(crate::ForwardingTable::default());
        let (module_tx, _module_rx) = tokio::sync::mpsc::channel(32);
        forwarding
            .register_module_connection(
                INCUMBENT,
                "a".into(),
                subc_protocol::PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                crate::FrameSink::new(module_tx),
            )
            .unwrap();
        let (client_tx, _client_rx) = tokio::sync::mpsc::channel(32);
        let relay = forwarding
            .begin_route_bind_relay_for_test(
                ConnectionId::new(30),
                crate::FrameSink::new(client_tx),
                1,
                "a",
            )
            .unwrap();
        forwarding
            .complete_pending_relay(
                INCUMBENT,
                relay.corr,
                crate::forwarding::RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        let _handler = crate::ControlHandler::with_forwarding(
            std::sync::Arc::new(crate::Registry::default()),
            forwarding.clone(),
        )
        .with_config_runtime(owner.clone());
        approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
        let view = pending(&owner, INCUMBENT);
        owner
            .install_approved_route_policy("a", view.generation)
            .await
            .unwrap();
        if timed_out {
            tokio::time::advance(Duration::from_secs(30)).await;
            owner
                .delivery_timeout(INCUMBENT, "a", view.generation, view.attempt.unwrap())
                .unwrap();
        } else {
            applied(&owner, INCUMBENT, &view, false).unwrap();
        }
        owner
            .install_approved_route_policy("a", view.generation)
            .await
            .unwrap();
        assert_eq!(forwarding.active_binding_count().unwrap(), 1);
        assert_active(&owner, INCUMBENT, 1);
    }
}

#[test]
fn config_admission_daemon_owned_route_rebind_is_refused_by_name() {
    let (rig, owner) = rig();
    let owner = std::sync::Arc::new(owner);
    let _handler = crate::ControlHandler::new(std::sync::Arc::new(crate::Registry::default()))
        .with_config_runtime(owner.clone());
    write(&rig, 2);
    let error = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), |old, new| {
            let mut changeset = daemon_review(old, new)?;
            changeset.fields.push(subc_config::FieldChange::new(
                "subc",
                "/scope_authority_owners",
                subc_config::RecordedValue::new(json!([])),
                subc_config::RecordedValue::new(json!(["a"])),
                subc_config::ApplyKind::RouteRebind,
            ));
            Ok(changeset)
        })
        .unwrap_err();
    assert_eq!(error.code, "daemon_route_policy_authority_unavailable");
}

fn plan_kinds_from_disk(
    owner: &RuntimeOwner,
    kinds: &[subc_config::ApplyKind],
) -> super::super::super::Result<ConfigPlanView> {
    owner.finish_plan(owner.prepare(PlanKind::Change)?, |old, new| {
        let mut changeset = daemon_review(old, new)?;
        for (index, kind) in kinds.iter().enumerate() {
            changeset.fields.push(subc_config::FieldChange::new(
                "a",
                format!("/field{index}"),
                subc_config::RecordedValue::new(json!(1)),
                subc_config::RecordedValue::new(json!(2)),
                *kind,
            ));
        }
        Ok(changeset)
    })
}

fn assert_active(owner: &RuntimeOwner, connection: ConnectionId, generation: u64) {
    assert_eq!(
        owner
            .module_get(connection, "a", ConfigGetRequest::new())
            .unwrap()
            .active
            .unwrap()
            .generation,
        generation
    );
}

async fn mixed_waits_for_spawn(kinds: &[subc_config::ApplyKind], expected: &str) {
    let (rig, owner) = rig();
    let owner = std::sync::Arc::new(
        owner.with_route_policy_authority(std::sync::Arc::new(FakeRouteAuthority::default())),
    );
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    let plan = approve_kinds(&rig, &owner, kinds);
    let transition = &plan.plan.changeset.apply_plan["module_transitions"]["a"];
    assert_eq!(transition["transition"], expected);
    assert_eq!(transition["whole_generation"], true);
    let deferred = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    assert!(
        deferred.pending.is_none(),
        "deferred bytes must never reach the old child"
    );
    assert_active(&owner, INCUMBENT, 1);
    assert!(owner.ready(INCUMBENT, "a", 2).unwrap().is_none());
    assert_active(&owner, INCUMBENT, 1);
    let spawn = owner
        .spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(120),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
    let replacement = ConnectionId::new(30);
    rig.declare(&owner, replacement.get());
    owner.registered(replacement, &spawn).unwrap();
    let view = pending(&owner, replacement);
    if owner.route_policy_needed("a", 2).unwrap() {
        owner.install_approved_route_policy("a", 2).await.unwrap();
    }
    assert_eq!(
        applied(&owner, replacement, &view, true).unwrap(),
        AppliedDisposition::WaitingForReadiness
    );
    assert_eq!(
        owner.ready(replacement, "a", 2).unwrap(),
        Some(AppliedDisposition::Active)
    );
    assert_active(&owner, replacement, 2);
}

#[tokio::test]
async fn config_delivery_mixed_next_supervision_and_route_rebind_wait_for_spawn() {
    mixed_waits_for_spawn(
        &[
            subc_config::ApplyKind::RouteRebind,
            subc_config::ApplyKind::NextSupervision,
        ],
        "next_supervision",
    )
    .await;
}

#[tokio::test]
async fn config_delivery_mixed_module_restart_and_next_supervision_wait_for_spawn() {
    mixed_waits_for_spawn(
        &[
            subc_config::ApplyKind::NextSupervision,
            subc_config::ApplyKind::ModuleRestart,
        ],
        "module_restart",
    )
    .await;
}

#[tokio::test]
async fn config_delivery_mixed_live_and_next_supervision_defer_live_fields_in_plan_and_activation()
{
    mixed_waits_for_spawn(
        &[
            subc_config::ApplyKind::Live,
            subc_config::ApplyKind::NextSupervision,
        ],
        "next_supervision",
    )
    .await;
    let (rig, owner) = rig();
    let plan = approve_kinds(
        &rig,
        &owner,
        &[
            subc_config::ApplyKind::Live,
            subc_config::ApplyKind::NextSupervision,
        ],
    );
    let transition = &plan.plan.changeset.apply_plan["module_transitions"]["a"];
    assert_eq!(transition["deferred_live_fields"], json!(["/field0"]));
    assert!(transition["explanation"]
        .as_str()
        .unwrap()
        .contains("waits for the module's next start"));
}

#[tokio::test]
async fn config_delivery_mixed_route_rebind_and_live_require_route_readiness_before_activation() {
    let (rig, owner) = rig();
    let owner = std::sync::Arc::new(
        owner.with_route_policy_authority(std::sync::Arc::new(FakeRouteAuthority::default())),
    );
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    let plan = approve_kinds(
        &rig,
        &owner,
        &[
            subc_config::ApplyKind::Live,
            subc_config::ApplyKind::RouteRebind,
        ],
    );
    assert_eq!(
        plan.plan.changeset.apply_plan["module_transitions"]["a"]["transition"],
        "route_rebind"
    );
    let view = pending(&owner, INCUMBENT);
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::WaitingForReadiness
    );
    assert_active(&owner, INCUMBENT, 1);
    assert!(owner.ready(INCUMBENT, "a", 2).unwrap().is_none());
    assert_active(&owner, INCUMBENT, 1);
    owner.install_approved_route_policy("a", 2).await.unwrap();
    assert_active(&owner, INCUMBENT, 1);
    owner.ready(INCUMBENT, "a", 2).unwrap();
    assert_active(&owner, INCUMBENT, 2);
}

#[test]
fn config_delivery_mixed_daemon_and_module_restart_waits_for_new_daemon_without_self_restart() {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    let plan = approve_kinds(
        &rig,
        &owner,
        &[
            subc_config::ApplyKind::ModuleRestart,
            subc_config::ApplyKind::DaemonRestart,
        ],
    );
    let transition = &plan.plan.changeset.apply_plan["module_transitions"]["a"];
    assert_eq!(transition["transition"], "daemon_restart");
    assert!(transition["explanation"]
        .as_str()
        .unwrap()
        .contains("when the daemon next restarts"));
    let deferred = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    assert!(deferred.pending.is_none());
    assert_active(&owner, INCUMBENT, 1);
    let status = owner.status().unwrap();
    assert_eq!(
        status.approved_but_inactive[0]["waiting_for"],
        "daemon_restart"
    );
    assert_eq!(
        status.approved_but_inactive[0]["digest"],
        plan.digest.to_string()
    );
    drop(owner);
    let restarted = rig.owner("new-daemon");
    let spawn = restarted
        .spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(120),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
    rig.declare(&restarted, 30);
    let connection = ConnectionId::new(30);
    restarted.registered(connection, &spawn).unwrap();
    let view = pending(&restarted, connection);
    assert_eq!(
        applied(&restarted, connection, &view, true).unwrap(),
        AppliedDisposition::WaitingForReadiness
    );
    assert_eq!(
        restarted.ready(connection, "a", 2).unwrap(),
        Some(AppliedDisposition::Active)
    );
    assert_active(&restarted, connection, 2);
}

#[test]
fn config_status_pending_only_view_is_gated_until_supervision_boundary_opens() {
    let (rig, owner) = rig();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::NextSupervision]);
    assert_eq!(
        owner
            .module_get(INCUMBENT, "a", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "config_gated"
    );
    assert_eq!(owner.status().unwrap().gated_modules, ["a"]);
    owner.disconnected(INCUMBENT);
    let spawn = owner
        .spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(120),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
    let replacement = ConnectionId::new(30);
    rig.declare(&owner, replacement.get());
    owner.registered(replacement, &spawn).unwrap();
    assert!(owner
        .module_get(replacement, "a", ConfigGetRequest::new())
        .unwrap()
        .pending
        .is_some());
    assert!(owner.status().unwrap().gated_modules.is_empty());
}

#[tokio::test(start_paused = true)]
async fn config_delivery_live_deadline_starts_at_write_and_enforces_both_edges() {
    let (_rig, owner) = rig();
    let reply = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    let view = reply.pending.as_ref().unwrap();
    tokio::time::advance(Duration::from_secs(120)).await;
    assert_eq!(
        applied(&owner, INCUMBENT, view, true).unwrap(),
        AppliedDisposition::Ignored
    );
    owner
        .module_view_delivered(INCUMBENT, "a", &reply, tokio::time::Instant::now(), true)
        .unwrap();
    tokio::time::advance(Duration::from_secs(30) - Duration::from_nanos(1)).await;
    assert_eq!(
        applied(&owner, INCUMBENT, view, true).unwrap(),
        AppliedDisposition::Active
    );
    let (_rig2, other) = rig();
    let view = pending(&other, INCUMBENT);
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(
        applied(&other, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::Failed {
            generation: 1,
            restore: None,
            keep_running: true,
        }
    );
    assert_eq!(
        applied(&other, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::Ignored
    );
}

#[tokio::test(start_paused = true)]
async fn config_delivery_replacement_warmup_uses_spawn_not_late_registration_and_both_edges() {
    for (elapsed, expected) in [
        (
            Duration::from_secs(135) - Duration::from_nanos(1),
            AppliedDisposition::Active,
        ),
        (
            Duration::from_secs(135),
            AppliedDisposition::Failed {
                generation: 2,
                restore: Some(1),
                keep_running: false,
            },
        ),
    ] {
        let (rig, owner) = rig();
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        approve_kinds(&rig, &owner, &[subc_config::ApplyKind::ModuleRestart]);
        let spawned_at = tokio::time::Instant::now();
        let spawn = owner
            .spawned(
                "a",
                spawned_at,
                Duration::from_secs(120),
                Duration::from_secs(5),
            )
            .unwrap()
            .unwrap();
        tokio::time::advance(Duration::from_secs(119)).await;
        let connection = ConnectionId::new(30);
        rig.declare(&owner, connection.get());
        owner.registered(connection, &spawn).unwrap();
        let view = pending(&owner, connection);
        owner.ready(connection, "a", 2).unwrap();
        tokio::time::advance(elapsed - Duration::from_secs(119)).await;
        assert_eq!(applied(&owner, connection, &view, true).unwrap(), expected);
    }
}

#[tokio::test(start_paused = true)]
async fn config_delivery_swap_just_ready_at_warmup_edge_gets_full_cutover_ack_window() {
    for (elapsed, expected) in [
        (
            Duration::from_secs(30) - Duration::from_nanos(1),
            AppliedDisposition::CatchupComplete,
        ),
        (
            Duration::from_secs(30),
            AppliedDisposition::CatchupFailed { active: 2 },
        ),
    ] {
        let (rig, owner) = rig();
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        candidate(&rig, &owner);
        owner
            .module_get(CANDIDATE, "a", ConfigGetRequest::new())
            .unwrap();
        tokio::time::advance(Duration::from_secs(135) - Duration::from_nanos(1)).await;
        approve(&rig, &owner, 2);
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        owner
            .promoted(CANDIDATE, "a", tokio::time::Instant::now())
            .unwrap()
            .unwrap();
        let view = pending(&owner, CANDIDATE);
        tokio::time::advance(elapsed).await;
        assert_eq!(applied(&owner, CANDIDATE, &view, true).unwrap(), expected);
    }
}

fn worker_fixture() -> (
    Rig,
    std::sync::Arc<RuntimeOwner>,
    std::sync::Arc<crate::ControlHandler>,
    Vec<tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>>,
) {
    worker_fixture_with_key(None)
}

fn worker_fixture_with_key(
    key: Option<&crate::config_store::TestStoreKey>,
) -> (
    Rig,
    std::sync::Arc<RuntimeOwner>,
    std::sync::Arc<crate::ControlHandler>,
    Vec<tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>>,
) {
    use std::sync::Arc;
    let rig = Rig::new();
    write(&rig, 1);
    std::fs::write(
        rig.path.join("cortexkit/b.jsonc"),
        b"{\"credential\":{\"vault_ref\":\"approved-reference\"},\"limit\":1}",
    )
    .unwrap();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&json!({"version":1,"modules":{
            "a":{"program":rig.path.join("program")}, "b":{"program":rig.path.join("program")}
        }}))
        .unwrap(),
    )
    .unwrap();
    let owner = Arc::new(match key {
        Some(key) => sealed_owner(&rig, key, "hint-worker"),
        None => rig.owner("hint-worker"),
    });
    rig.declare(&owner, INCUMBENT.get());
    owner
        .declare(
            ConnectionId::new(2),
            "b",
            subc_protocol::config::ConfigDeclareRequest::new(
                rig.declaration.schema.clone(),
                rig.path.join("cortexkit/b.jsonc").to_string_lossy(),
                vec![],
            ),
            true,
        )
        .unwrap();
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let mut receivers = vec![];
    for (id, module, opted) in [
        (INCUMBENT.get(), "a", true),
        (2, "b", true),
        (3, "c", false),
    ] {
        let connection = ConnectionId::new(id);
        registry
            .register_with_control_ops(
                subc_protocol::manifest::ModuleManifest::builder(module, "0.1.0").build(),
                subc_protocol::PROTOCOL_VERSION,
                connection,
                if opted {
                    vec!["config.review".into()]
                } else {
                    vec![]
                },
            )
            .unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        forwarding
            .register_module_connection(
                connection,
                module.into(),
                subc_protocol::PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                crate::FrameSink::new(sender),
            )
            .unwrap();
        receivers.push(receiver);
    }
    let handler = Arc::new(
        crate::ControlHandler::with_forwarding(registry, forwarding)
            .with_config_runtime(owner.clone()),
    );
    (rig, owner, handler, receivers)
}

#[tokio::test]
async fn config_delivery_io_worker_checks_keychain_and_store_hash_off_the_runtime_thread() {
    let key = crate::config_store::TestStoreKey::default();
    let (_rig, owner, handler, mut receivers) = worker_fixture_with_key(Some(&key));
    let threads = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    owner
        .lock()
        .unwrap()
        .store
        .observe_io_threads(threads.clone());
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    tokio::time::timeout(Duration::from_secs(5), receivers[0].recv())
        .await
        .unwrap()
        .unwrap();
    let seen = threads.lock().unwrap();
    assert!(seen.iter().any(|(op, _)| *op == "keychain_load"));
    assert!(seen.iter().any(|(op, _)| *op == "store_hash"));
    assert!(seen.iter().all(|(_, thread)| *thread != std::thread::current().id()),
        "the real delivery worker must never check sealed authority on the async runtime thread: {seen:?}");
}

#[tokio::test]
async fn config_delivery_io_get_checks_keychain_and_store_hash_off_the_runtime_thread() {
    let key = crate::config_store::TestStoreKey::default();
    let (_rig, owner, handler, _) = worker_fixture_with_key(Some(&key));
    let threads = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    owner
        .lock()
        .unwrap()
        .store
        .observe_io_threads(threads.clone());
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    let context = crate::router::RouteCtx {
        connection_id: INCUMBENT,
        egress: crate::FrameSink::new(sender),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Request,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        19,
        serde_json::to_vec(&ConfigGetRequest::new()).unwrap(),
    )
    .unwrap();
    let task = tokio::spawn(async move { handler.handle_control_frame(&context, frame).await });
    let mut response = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    response.flushed.take().unwrap().send(()).unwrap();
    assert!(task.await.unwrap().unwrap().is_empty());
    let seen = threads.lock().unwrap();
    assert!(seen.iter().any(|(op, _)| *op == "keychain_load"));
    assert!(seen.iter().any(|(op, _)| *op == "store_hash"));
    assert!(seen.iter().all(|(_, thread)| *thread != std::thread::current().id()),
        "the real get handler must never check sealed authority on the async runtime thread: {seen:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_spawn_async_io_checks_keychain_and_store_hash_off_the_runtime_thread() {
    use std::{os::unix::fs::PermissionsExt, sync::Arc};
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("program"),
        b"#!/bin/sh\nwhile :; do /bin/sleep 600; done\n",
    )
    .unwrap();
    std::fs::set_permissions(
        rig.path.join("program"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    write(&rig, 1);
    let key = crate::config_store::TestStoreKey::default();
    let owner = Arc::new(sealed_owner(&rig, &key, "async-spawn-io"));
    rig.declare(&owner, INCUMBENT.get());
    rig.stamp(&owner);
    let supervisor = crate::supervise::Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        crate::supervise::RestartPolicy::new(3, Duration::ZERO),
    )
    .with_config_runtime(owner.clone())
    .with_drain_timeout(Duration::ZERO);
    let threads = Arc::new(std::sync::Mutex::new(vec![]));
    owner
        .lock()
        .unwrap()
        .store
        .observe_io_threads(threads.clone());
    let module = supervisor
        .spawn_async(supervised_spec(&rig, "a"))
        .await
        .unwrap();
    {
        let seen = threads.lock().unwrap();
        assert!(seen.iter().any(|(op, _)| *op == "keychain_load"));
        assert!(seen.iter().any(|(op, _)| *op == "store_hash"));
        assert!(
            seen.iter()
                .all(|(_, thread)| *thread != std::thread::current().id()),
            "async launch must check sealed authority only on blocking workers: {seen:?}"
        );
    }
    module.stop().await.unwrap();
}

#[test]
fn config_callback_queues_are_independent_of_the_store_mutex() {
    let (_rig, owner) = rig();
    let owner = std::sync::Arc::new(owner);
    owner.approval_request_index.lock().unwrap().insert(
        (INCUMBENT, 91),
        crate::config_gate::runtime::ApprovalRequest::for_attempt(17),
    );
    let (sender, _) = tokio::sync::mpsc::channel(1);
    let cancel = subc_protocol::Frame::build(
        subc_protocol::FrameType::Cancel,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        91,
        vec![],
    )
    .unwrap();
    let locked = owner.inner.lock().unwrap();
    let callback_owner = owner.clone();
    let (done, observed) = std::sync::mpsc::channel();
    let callbacks = std::thread::spawn(move || {
        assert!(callback_owner.cancel_approval_request(INCUMBENT, 91));
        callback_owner.disconnected(INCUMBENT);
        callback_owner.queue_phone_withdrawal(INCUMBENT, crate::FrameSink::new(sender), cancel);
        done.send(()).unwrap();
    });
    let completed_while_locked = observed.recv_timeout(Duration::from_secs(1));
    drop(locked);
    callbacks.join().unwrap();
    assert!(
        completed_while_locked.is_ok(),
        "reader and Drop callbacks must finish while the store mutex is still held"
    );
    assert!(!owner.lock().unwrap().declarations.contains_key(&INCUMBENT));
    assert_eq!(owner.take_phone_withdrawals().len(), 1);
    owner.phone_withdrawn(INCUMBENT);
}

struct WorkerGuard(tokio::task::JoinHandle<()>);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn config_delivery_worker_isolates_stuck_flush_and_never_hints_undeclared_module() {
    let _clock = hold_test_clock();
    let (_rig, owner, handler, mut receivers) = worker_fixture();
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    let mut a = receivers.remove(0);
    let mut b = receivers.remove(0);
    let mut c = receivers.remove(0);
    let mut blocked = a.recv().await.unwrap();
    let mut delivered = b.recv().await.unwrap();
    let frame = &delivered.frame;
    assert_eq!(frame.header.ty, subc_protocol::FrameType::Push);
    assert_eq!(
        (frame.header.channel, frame.header.epoch, frame.header.corr),
        (0, 0, 0)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&frame.body).unwrap()["op"],
        "config.changed"
    );
    delivered.flushed.take().unwrap().send(()).unwrap();
    wait_for_blocking_delivery(|| {
        owner
            .delivery_window(ConnectionId::new(2))
            .unwrap()
            .is_some_and(|window| window.delivered)
    })
    .await;
    let reply = owner
        .module_get(ConnectionId::new(2), "b", ConfigGetRequest::new())
        .unwrap();
    let view = reply.pending.unwrap();
    assert_eq!(
        owner
            .module_applied(
                ConnectionId::new(2),
                "b",
                ConfigAppliedRequest::new(
                    view.generation,
                    view.attempt.unwrap(),
                    ConfigApplyResult::Ack,
                    vec![]
                )
            )
            .unwrap(),
        AppliedDisposition::Active
    );
    assert!(matches!(
        c.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for_blocking_delivery(|| {
        owner
            .module_get(INCUMBENT, "a", ConfigGetRequest::new())
            .is_err()
    })
    .await;
    assert!(
        blocked.flushed.take().unwrap().send(()).is_err(),
        "late flush cannot resurrect the timed-out attempt"
    );
    assert_eq!(
        owner
            .module_get(INCUMBENT, "a", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "config_gated"
    );
    assert!(owner
        .module_get(ConnectionId::new(2), "b", ConfigGetRequest::new())
        .unwrap()
        .active
        .is_some());
    assert!(matches!(
        c.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test(start_paused = true)]
async fn config_delivery_worker_anchor_recovery_hints_refused_connection_once() {
    worker_anchor_recovery_hint(false).await;
}

#[tokio::test(start_paused = true)]
async fn config_delivery_worker_operator_anchor_recovery_hints_refused_connection_once() {
    worker_anchor_recovery_hint(true).await;
}

async fn worker_anchor_recovery_hint(operator_command: bool) {
    let _clock = hold_test_clock();
    let key = crate::config_store::TestStoreKey::default();
    let (rig, owner, handler, mut receivers) = worker_fixture_with_key(Some(&key));
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    let mut a = receivers.remove(0);
    let mut b = receivers.remove(0);
    let mut undeclared = receivers.remove(0);
    for receiver in [&mut a, &mut b] {
        receiver
            .recv()
            .await
            .unwrap()
            .flushed
            .take()
            .unwrap()
            .send(())
            .unwrap();
    }
    wait_for_blocking_delivery(|| {
        owner
            .delivery_window(INCUMBENT)
            .unwrap()
            .is_some_and(|w| w.delivered)
            && owner
                .delivery_window(ConnectionId::new(2))
                .unwrap()
                .is_some_and(|w| w.delivered)
    })
    .await;
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    let reply = owner
        .module_get(ConnectionId::new(2), "b", ConfigGetRequest::new())
        .unwrap();
    owner
        .module_view_delivered(
            ConnectionId::new(2),
            "b",
            &reply,
            tokio::time::Instant::now(),
            true,
        )
        .unwrap();
    let view = reply.pending.unwrap();
    owner
        .module_applied(
            ConnectionId::new(2),
            "b",
            ConfigAppliedRequest::new(
                view.generation,
                view.attempt.unwrap(),
                ConfigApplyResult::Ack,
                vec![],
            ),
        )
        .unwrap();
    approve(&rig, &owner, 2);
    owner.notify_delivery_changed();
    wait_for_blocking_delivery(|| !a.is_empty()).await;
    a.recv()
        .await
        .unwrap()
        .flushed
        .take()
        .unwrap()
        .send(())
        .unwrap();
    let view = pending(&owner, INCUMBENT);
    key.fail_anchor(true);
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap_err().code,
        "anchor_pending"
    );
    owner.anchor_pending_served(INCUMBENT).unwrap();
    owner.anchor_pending_served(ConnectionId::new(3)).unwrap();
    owner.anchor_pending_served(ConnectionId::new(2)).unwrap();
    owner.disconnected(ConnectionId::new(2));
    owner
        .declare(
            ConnectionId::new(44),
            "b",
            subc_protocol::config::ConfigDeclareRequest::new(
                rig.declaration.schema.clone(),
                rig.path.join("cortexkit/b.jsonc").to_string_lossy(),
                vec![],
            ),
            true,
        )
        .unwrap();
    owner.retry_anchor().unwrap();
    key.fail_anchor(false);
    if operator_command {
        assert!(!owner.status().unwrap().anchor_pending);
    } else {
        owner.notify_delivery_changed();
        tokio::time::advance(Duration::from_secs(2)).await;
        wait_for_blocking_delivery(|| !owner.lock().unwrap().store.status().anchor_pending).await;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while a.is_empty() && std::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert!(
        !a.is_empty(),
        "recovered anchor must hint the refused connection (operator command: {operator_command})"
    );
    let mut recovered = a.recv().await.unwrap();
    assert_eq!(recovered.frame.header.ty, subc_protocol::FrameType::Push);
    assert_eq!(
        (
            recovered.frame.header.channel,
            recovered.frame.header.epoch,
            recovered.frame.header.corr
        ),
        (0, 0, 0)
    );
    let hint: subc_protocol::config::ConfigChanged =
        serde_json::from_slice(&recovered.frame.body).unwrap();
    assert_eq!(hint.generation, 2);
    assert!(hint.fields.is_empty());
    recovered.flushed.take().unwrap().send(()).unwrap();
    wait_for_blocking_delivery(|| !owner.lock().unwrap().store.status().anchor_pending).await;
    assert!(owner.take_anchor_hint(INCUMBENT).unwrap().is_none());
    assert!(owner
        .take_anchor_hint(ConnectionId::new(2))
        .unwrap()
        .is_none());
    assert!(owner
        .take_anchor_hint(ConnectionId::new(44))
        .unwrap()
        .is_none());
    assert!(a.try_recv().is_err());
    assert!(b.try_recv().is_err());
    assert!(undeclared.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn config_delivery_worker_flush_at_29_9_seconds_starts_a_full_ack_window() {
    let _clock = hold_test_clock();
    let (_rig, owner, handler, mut receivers) = worker_fixture();
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    let mut a = receivers.remove(0);
    let mut outbound = a.recv().await.unwrap();
    tokio::time::advance(Duration::from_millis(29_900)).await;
    outbound.flushed.take().unwrap().send(()).unwrap();
    wait_for_blocking_delivery(|| {
        owner
            .delivery_window(INCUMBENT)
            .unwrap()
            .is_some_and(|window| window.delivered)
    })
    .await;
    let view = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap()
        .pending
        .unwrap();
    tokio::time::advance(Duration::from_secs(30) - Duration::from_nanos(1)).await;
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::Active
    );
}

#[tokio::test]
async fn config_delivery_spontaneous_exit_fences_only_its_module_until_failure_is_recorded() {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve(&rig, &owner, 2);
    let spawn = owner
        .delivery_spawned_in_slot(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            true,
        )
        .unwrap()
        .unwrap();
    owner.delivery_child_exited(&spawn).unwrap();
    let mut fence = owner.delivery_exit_fence("a").unwrap().unwrap();
    assert!(*fence.borrow());
    assert!(owner.delivery_exit_fence("b").unwrap().is_none());
    owner.disconnected(INCUMBENT);
    let exit = owner.delivery_exits().unwrap().pop().unwrap();
    let gate = owner.module_gate("a").unwrap();
    let _guard = gate.lock().await;
    assert!(
        !owner.delivery_record_exit(&exit).unwrap(),
        "an anchored active can be respawned"
    );
    assert!(
        *fence.borrow(),
        "recording must not release the spawn fence before the supervisor is reconciled"
    );
    owner.delivery_release_exit(&exit).unwrap();
    fence.changed().await.unwrap();
    assert!(!*fence.borrow());
    rig.declare(&owner, CANDIDATE.get());
    let reply = owner
        .module_get(CANDIDATE, "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(reply.active.unwrap().generation, 1);
    assert!(
        reply.pending.is_none(),
        "the crashed generation cannot be redelivered"
    );
}

#[tokio::test]
async fn config_delivery_transport_eof_and_intentional_restart_preserve_the_pending_generation() {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    let generation = approve(&rig, &owner, 2);
    let old_spawn = owner
        .delivery_spawned_in_slot(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            true,
        )
        .unwrap()
        .unwrap();
    owner
        .delivery_child_registered(INCUMBENT, &old_spawn)
        .unwrap();
    let old = pending(&owner, INCUMBENT);
    owner.disconnected(INCUMBENT);
    assert!(owner.delivery_exits().unwrap().is_empty());
    assert!(owner.delivery_exit_fence("a").unwrap().is_none());
    rig.declare(&owner, CANDIDATE.get());
    let replacement = owner
        .delivery_spawned_in_slot(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            true,
        )
        .unwrap()
        .unwrap();
    owner
        .delivery_child_registered(CANDIDATE, &replacement)
        .unwrap();
    let next = pending(&owner, CANDIDATE);
    assert_eq!(next.generation, generation);
    assert_ne!(next.attempt, old.attempt);
}

#[tokio::test]
async fn config_delivery_pre_hello_failure_after_stamp_holds_without_an_active_generation() {
    let (_rig, owner) = rig();
    let spawn = owner
        .delivery_spawned_in_slot(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            true,
        )
        .unwrap()
        .unwrap();
    owner.delivery_child_exited(&spawn).unwrap();
    let exit = owner.delivery_exits().unwrap().pop().unwrap();
    let gate = owner.module_gate("a").unwrap();
    let _guard = gate.lock().await;
    assert!(owner.delivery_record_exit(&exit).unwrap());
    owner.delivery_release_exit(&exit).unwrap();
    assert_eq!(
        owner
            .module_get(INCUMBENT, "a", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "config_gated"
    );
}

#[cfg(unix)]
fn supervised_spec(rig: &Rig, module: &str) -> crate::supervise::ModuleSpec {
    crate::supervise::ModuleSpec {
        module_id: module.into(),
        program: rig.path.join("program"),
        args: vec![
            rig.path
                .join(format!("{module}.seen"))
                .to_string_lossy()
                .into_owned(),
            rig.path
                .join(format!("{module}.launches"))
                .to_string_lossy()
                .into_owned(),
        ],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: subc_control::ModuleProtocol::None,
        overlap: crate::supervise::ModuleOverlap::Exclusive,
        carry_containment: false,
    }
}

#[cfg(unix)]
fn supervised_fixture(
    has_active: bool,
) -> (
    Rig,
    std::sync::Arc<RuntimeOwner>,
    crate::supervise::Supervisor,
    std::sync::Arc<crate::ControlHandler>,
) {
    supervised_fixture_with_carry(has_active, false)
}

#[cfg(unix)]
fn supervised_fixture_with_carry(
    has_active: bool,
    carry_containment: bool,
) -> (
    Rig,
    std::sync::Arc<RuntimeOwner>,
    crate::supervise::Supervisor,
    std::sync::Arc<crate::ControlHandler>,
) {
    supervised_fixture_with_program(has_active, carry_containment, b"#!/bin/sh\nprintf 'started\\n' >> \"$2\"\nif [ ! -e \"$1\" ]; then : > \"$1\"; exit 23; fi\nwhile :; do /bin/sleep 600; done\n")
}

#[cfg(unix)]
fn supervised_fixture_with_program(
    has_active: bool,
    carry_containment: bool,
    program: &[u8],
) -> (
    Rig,
    std::sync::Arc<RuntimeOwner>,
    crate::supervise::Supervisor,
    std::sync::Arc<crate::ControlHandler>,
) {
    use std::{os::unix::fs::PermissionsExt, sync::Arc};
    let rig = Rig::new();
    std::fs::write(rig.path.join("program"), program).unwrap();
    std::fs::set_permissions(
        rig.path.join("program"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    write(&rig, 1);
    let path = rig.path.join("subc.jsonc");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for module in ["a", "b"] {
        doc["modules"][module] = json!({"program":rig.path.join("program"),"args":[rig.path.join(format!("{module}.seen")),rig.path.join(format!("{module}.launches"))],"protocol":"subc","overlap":if carry_containment {"exclusive"}else{"safe"},"carry_containment":carry_containment});
    }
    doc["modules"]["a"]["carry_containment"] = json!(carry_containment);
    std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let owner = Arc::new(rig.owner("supervised-delivery"));
    rig.declare(&owner, INCUMBENT.get());
    rig.stamp(&owner);
    if has_active {
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        approve(&rig, &owner, 2);
    }
    let registry = Arc::new(crate::Registry::default());
    let handle = crate::supervise::SupervisorHandle::new();
    // Keep the channel available during manual delivery and warmup. A missing
    // registration service would be a separate launch failure, not warmup.
    handle.capture_registration_channels_for_test();
    let supervisor = crate::supervise::Supervisor::new_for_test(
        registry.clone(),
        crate::supervise::RestartPolicy {
            max_restarts: 3,
            backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            window: Duration::from_secs(60),
        },
    )
    .with_handle(handle.clone())
    .with_config_runtime(owner.clone())
    .with_drain_timeout(Duration::from_millis(20));
    let handler = Arc::new(
        crate::ControlHandler::new(registry)
            .with_supervisor(handle)
            .with_config_runtime(owner.clone()),
    );
    (rig, owner, supervisor, handler)
}

#[cfg(unix)]
impl RuntimeOwner {
    pub(crate) fn unactivated_supervisor_fixture_for_test() -> (
        impl Drop,
        std::sync::Arc<Self>,
        crate::supervise::Supervisor,
        std::sync::Arc<crate::ControlHandler>,
        crate::supervise::ModuleSpec,
    ) {
        Self::unactivated_supervisor_fixture_with_carry_for_test(false)
    }

    pub(crate) fn unactivated_supervisor_fixture_with_carry_for_test(
        carry: bool,
    ) -> (
        impl Drop,
        std::sync::Arc<Self>,
        crate::supervise::Supervisor,
        std::sync::Arc<crate::ControlHandler>,
        crate::supervise::ModuleSpec,
    ) {
        let (rig, owner, supervisor, handler) = supervised_fixture_with_carry(false, carry);
        let path = rig.path.join("subc.jsonc");
        let (config, _) = crate::daemon_config::parse_with_activity(
            &std::fs::read_to_string(&path).unwrap(),
            &path,
        )
        .unwrap();
        let spec = config
            .modules
            .into_iter()
            .find(|entry| entry.module_id == "a")
            .unwrap()
            .module_spec();
        (rig, owner, supervisor, handler, spec)
    }
}

#[cfg(unix)]
async fn wait_for_delivery_condition(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("supervised delivery condition was not reached");
}

#[cfg(unix)]
async fn wait_for_supervised_stage(
    module: &crate::supervise::SupervisedModule,
    stage: &str,
    mut condition: impl FnMut() -> bool,
) {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "{stage}: status {:?}; terminal history {:?}; stderr {:?}",
        module.status(),
        module.terminal_history(),
        module.stderr_tail(None, None)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn config_delivery_supervisor_pre_hello_crash_defers_respawn_without_spending_budget_and_restores_active(
) {
    let (rig, owner, supervisor, handler) = supervised_fixture(true);
    let module = supervisor.spawn(supervised_spec(&rig, "a")).unwrap();
    wait_for_supervised_stage(&module, "pre-HELLO crash raises exit fence", || {
        owner.delivery_exit_fence("a").unwrap().is_some()
    })
    .await;
    wait_for_supervised_stage(&module, "pre-HELLO crash defers the actor", || {
        module.state().unwrap() == crate::supervise::ModuleState::Restarting
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(rig.path.join("a.launches"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        module.status().unwrap().restart_count,
        1,
        "the real crash spends one entry, waiting on its fence spends none"
    );
    let probe = crate::supervise::Supervisor::new_for_test(
        std::sync::Arc::new(crate::Registry::default()),
        crate::supervise::RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let deferred = probe.spawn(supervised_spec(&rig, "a")).unwrap();
    let deferred_state = deferred.state().unwrap();
    let deferred_budget = deferred.status().unwrap().restart_count;
    deferred.stop().await.unwrap();
    assert_eq!(
        deferred_state,
        crate::supervise::ModuleState::Restarting,
        "the common synchronous spawn path must defer before exec"
    );
    assert_eq!(deferred_budget, 0, "a refused spawn is not a crash");
    assert_eq!(
        std::fs::read_to_string(rig.path.join("a.launches"))
            .unwrap()
            .lines()
            .count(),
        1,
        "no failed-generation process may start while the fence is raised"
    );
    std::fs::write(rig.path.join("b.seen"), b"keep running").unwrap();
    let other = supervisor.spawn(supervised_spec(&rig, "b")).unwrap();
    wait_for_delivery_condition(|| rig.path.join("b.launches").exists()).await;
    assert_eq!(
        other.state().unwrap(),
        crate::supervise::ModuleState::Running
    );
    assert!(owner.delivery_exit_fence("a").unwrap().is_some());
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    wait_for_supervised_stage(
        &module,
        "pre-HELLO fence clears and one replacement starts",
        || {
            std::fs::read_to_string(rig.path.join("a.launches"))
                .is_ok_and(|text| text.lines().count() == 2)
        },
    )
    .await;
    assert!(owner.delivery_exit_fence("a").unwrap().is_none());
    assert_eq!(module.status().unwrap().restart_count, 1);
    rig.declare(&owner, 30);
    let view = owner
        .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(view.active.unwrap().generation, 1);
    assert!(
        view.pending.is_none(),
        "the replacement sees restored active, not the failed generation"
    );
    module.stop().await.unwrap();
    other.stop().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_delivery_supervisor_pre_hello_crash_after_unavailable_witness_spends_budget() {
    exit_after_unavailable_image("after_content_witness_check", true).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_delivery_supervisor_pre_hello_crash_after_unavailable_initial_image_spends_budget()
{
    exit_after_unavailable_image("after_spawn_image_sample", true).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_delivery_supervisor_unavailable_witness_admission_kill_spends_no_budget() {
    exit_after_unavailable_image("after_content_witness_check", false).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_delivery_supervisor_unavailable_initial_image_admission_kill_spends_no_budget() {
    exit_after_unavailable_image("after_spawn_image_sample", false).await;
}

#[cfg(target_os = "linux")]
async fn exit_after_unavailable_image(unavailable_stage: &'static str, child_exits: bool) {
    use std::io::Write;
    let (rig, owner, supervisor, _handler) = supervised_fixture_with_program(
        true,
        false,
        b"#!/bin/sh\nprintf 'started\\n' >> \"$2\"\nread -r release < \"$1\"\nexit 23\n",
    );
    let release = rig.path.join("a.seen");
    assert!(std::process::Command::new("mkfifo")
        .arg(&release)
        .status()
        .unwrap()
        .success());
    let checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let checked_hook = checked.clone();
    let kill_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let kill_sent_hook = kill_sent.clone();
    let launched = rig.path.join("a.launches");
    supervisor.set_admission_hook_for_test(move |module, stage, pid| {
        if module != "a" {
            return true;
        }
        match stage {
            "after_admission_kill_sent" => {
                kill_sent_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }
            // Model a transient /proc image read failure while the real program
            // is blocked on its release pipe, not yet waitable as an exited child.
            stage if stage == unavailable_stage => {
                assert!(subc_os::child_exit::observe(pid.unwrap())
                    .unwrap()
                    .is_none());
                checked_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                false
            }
            "before_content_witness_kill" | "before_initial_image_kill" => {
                assert!(checked_hook.load(std::sync::atomic::Ordering::SeqCst));
                if !child_exits {
                    // Let the program reach its FIFO before allowing the kill,
                    // so this control also proves that one real launch occurred.
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !std::fs::read_to_string(&launched).is_ok_and(|text| text == "started\n")
                    {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "program did not start"
                        );
                        std::thread::yield_now();
                    }
                    return true;
                }
                writeln!(std::fs::File::create(&release).unwrap(), "crash").unwrap();
                // Finish the program's own exit before the supervisor can signal
                // it. WNOWAIT observes the status without stealing its reap.
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(exit) = subc_os::child_exit::observe(pid.unwrap()).unwrap() {
                        assert_eq!(exit.code(), Some(23));
                        break;
                    }
                    assert!(std::time::Instant::now() < deadline, "program did not exit");
                    std::thread::yield_now();
                }
                true
            }
            _ => true,
        }
    });
    let module = supervisor.spawn(supervised_spec(&rig, "a")).unwrap();
    wait_for_supervised_stage(&module, "unavailable-witness crash defers recovery", || {
        module.state().unwrap() == crate::supervise::ModuleState::Restarting
    })
    .await;
    assert!(checked.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        kill_sent.load(std::sync::atomic::Ordering::SeqCst),
        "the admission kill must have been sent, even if the program's own exit won"
    );
    let status = module.status().unwrap();
    if child_exits {
        assert_eq!(
            status.restart_count, 1,
            "a real exit 23 after exec spends one entry even when its witness was unavailable"
        );
        assert_eq!(status.lifetime_restarts, 1);
        assert_eq!(status.last_exit.unwrap().code, Some(23));
    } else {
        assert_eq!(status.restart_count, 0, "an admission kill is not a crash");
        assert_eq!(status.lifetime_restarts, 0);
        assert_eq!(status.last_exit.unwrap().signal, Some(9));
    }
    assert!(!status.process_alive);
    assert!(owner.delivery_exit_fence("a").unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(rig.path.join("a.launches"))
            .unwrap()
            .lines()
            .count(),
        1,
        "the exit fence must prevent a second exec"
    );
    let probe = crate::supervise::Supervisor::new_for_test(
        std::sync::Arc::new(crate::Registry::default()),
        crate::supervise::RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let deferred = probe.spawn(supervised_spec(&rig, "a")).unwrap();
    assert_eq!(deferred.status().unwrap().restart_count, 0);
    deferred.stop().await.unwrap();
    module.stop().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn config_delivery_supervisor_operator_restart_during_warmup_keeps_pending_for_the_next_child(
) {
    let (rig, owner, supervisor, _handler) = supervised_fixture(true);
    std::fs::write(rig.path.join("a.seen"), b"keep running").unwrap();
    let module = supervisor.spawn(supervised_spec(&rig, "a")).unwrap();
    wait_for_delivery_condition(|| rig.path.join("a.launches").exists()).await;
    module.restart(Some(20)).await.unwrap();
    wait_for_delivery_condition(|| {
        std::fs::read_to_string(rig.path.join("a.launches"))
            .is_ok_and(|text| text.lines().count() == 2)
    })
    .await;
    assert!(owner.delivery_exits().unwrap().is_empty());
    assert!(owner.delivery_exit_fence("a").unwrap().is_none());
    rig.declare(&owner, 30);
    let view = owner
        .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(view.pending.unwrap().generation, 2);
    assert_eq!(
        module.status().unwrap().restart_count,
        0,
        "an intentional operator restart does not consume crash budget"
    );
    module.stop().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn config_delivery_supervisor_pre_hello_failure_without_active_stops_before_releasing_the_fence(
) {
    let (rig, owner, supervisor, handler) = supervised_fixture(false);
    let module = supervisor.spawn(supervised_spec(&rig, "a")).unwrap();
    wait_for_delivery_condition(|| owner.delivery_exit_fence("a").unwrap().is_some()).await;
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    wait_for_delivery_condition(|| owner.delivery_exit_fence("a").unwrap().is_none()).await;
    assert_eq!(
        module.state().unwrap(),
        crate::supervise::ModuleState::Stopped
    );
    assert_eq!(
        std::fs::read_to_string(rig.path.join("a.launches"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        owner
            .module_get(INCUMBENT, "a", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "config_gated"
    );
    module.stop().await.unwrap();
}

#[tokio::test]
async fn config_delivery_exit_fence_restores_memory_under_anchor_pending_and_never_redelivers_after_retry(
) {
    let rig = Rig::new();
    write(&rig, 1);
    let key = crate::config_store::TestStoreKey::default();
    let owner = sealed_owner(&rig, &key, "exit-anchor-pending");
    rig.declare(&owner, INCUMBENT.get());
    rig.stamp(&owner);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve(&rig, &owner, 2);
    let spawn = owner
        .delivery_spawned_in_slot(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            true,
        )
        .unwrap()
        .unwrap();
    key.fail_anchor(true);
    write(&rig, 3);
    assert_eq!(owner.status().unwrap_err().code, "anchor_pending");
    owner.delivery_child_exited(&spawn).unwrap();
    let exit = owner.delivery_exits().unwrap().pop().unwrap();
    {
        let gate = owner.module_gate("a").unwrap();
        let _guard = gate.lock().await;
        assert!(!owner.delivery_record_exit(&exit).unwrap());
        owner.delivery_release_exit(&exit).unwrap();
    }
    {
        let mut state = owner.lock().unwrap();
        let body = state.store.body().unwrap();
        assert_eq!(
            (body.versions[0].1.active, body.versions[0].1.pending),
            (Some(1), None)
        );
        assert_eq!(body.generations[&2].generation.never_redeliver, vec!["a"]);
    }
    key.fail_anchor(false);
    assert!(!owner.status().unwrap().anchor_pending);
    drop(owner);
    let restarted = sealed_owner(&rig, &key, "exit-failure-anchored");
    rig.declare(&restarted, INCUMBENT.get());
    let reply = restarted
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(reply.active.unwrap().generation, 1);
    assert!(reply.pending.is_none());
}

#[test]
fn config_delivery_remembers_first_healthy_probe_before_the_replacement_gets_its_attempt() {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::ModuleRestart]);
    let spawn = owner
        .spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .unwrap()
        .unwrap();
    let replacement = ConnectionId::new(30);
    owner.registered(replacement, &spawn).unwrap();
    rig.declare(&owner, replacement.get());
    assert!(owner.ready(replacement, "a", 2).unwrap().is_none());
    let view = pending(&owner, replacement);
    assert_eq!(applied(&owner, replacement, &view, true).unwrap(), AppliedDisposition::Active,
        "the module must not need an extra health probe just because its first probe beat config.get");
}

#[test]
fn config_delivery_an_old_child_crash_preserves_a_generation_deferred_until_its_next_start() {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    let old = owner
        .delivery_spawned_in_slot(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            true,
        )
        .unwrap()
        .unwrap();
    owner.registered(INCUMBENT, &old).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::NextSupervision]);
    assert!(owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap()
        .pending
        .is_none());
    owner.delivery_child_exited(&old).unwrap();
    assert!(
        owner.delivery_exits().unwrap().is_empty(),
        "an old child never attempted the generation reserved for its successor"
    );
    let replacement = owner
        .spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .unwrap()
        .unwrap();
    rig.declare(&owner, 30);
    owner
        .registered(ConnectionId::new(30), &replacement)
        .unwrap();
    assert_eq!(pending(&owner, ConnectionId::new(30)).generation, 2);
}

#[test]
fn config_delivery_stamp_attempt_survives_transport_cleanup_until_its_process_exits() {
    let rig = Rig::new();
    write(&rig, 1);
    let owner = rig.owner("stamp-after-launch");
    let old = owner
        .delivery_spawned_in_slot(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            true,
        )
        .unwrap()
        .unwrap();
    rig.declare(&owner, INCUMBENT.get());
    rig.stamp(&owner);
    assert_eq!(pending(&owner, INCUMBENT).generation, 1);
    owner.disconnected(INCUMBENT);
    assert!(
        owner.delivery_exits().unwrap().is_empty(),
        "transport cleanup alone never fails the attempt"
    );
    owner.delivery_child_exited(&old).unwrap();
    assert_eq!(
        owner.delivery_exits().unwrap().len(),
        1,
        "the later process exit must still fail its assigned stamp generation"
    );
}

#[tokio::test(start_paused = true)]
async fn config_delivery_get_flush_timeout_fails_the_attempt_without_failing_the_connection() {
    let _clock = hold_test_clock();
    let (_rig, owner, handler, _receivers) = worker_fixture();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    let sink = crate::FrameSink::new(sender);
    let context = crate::router::RouteCtx {
        connection_id: INCUMBENT,
        egress: sink.clone(),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Request,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        17,
        serde_json::to_vec(&ConfigGetRequest::new()).unwrap(),
    )
    .unwrap();
    let task = tokio::spawn(async move { handler.handle_control_frame(&context, frame).await });
    let mut outbound = receiver
        .recv()
        .await
        .expect("config.get response was queued");
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(
        task.await.unwrap().unwrap().is_empty(),
        "a write timeout must not return the transport error which ends this connection"
    );
    assert!(outbound.flushed.take().unwrap().send(()).is_err());
    assert!(!sink.is_closed());
    assert_eq!(
        owner
            .module_get(INCUMBENT, "a", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "config_gated"
    );
}

#[tokio::test]
async fn config_delivery_get_releases_the_gate_and_discards_a_superseded_flush_receipt() {
    let (rig, owner, handler, _receivers) = worker_fixture();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    let context = crate::router::RouteCtx {
        connection_id: INCUMBENT,
        egress: crate::FrameSink::new(sender),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Request,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        18,
        serde_json::to_vec(&ConfigGetRequest::new()).unwrap(),
    )
    .unwrap();
    let task = tokio::spawn(async move { handler.handle_control_frame(&context, frame).await });
    let mut old = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let gate = owner.module_gate("a").unwrap();
    let guard = gate
        .try_lock()
        .expect("config.get must not hold the module gate while the module writer is stalled");
    approve(&rig, &owner, 2);
    let fresh = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(fresh.pending.as_ref().unwrap().generation, 2);
    owner
        .module_view_delivered(INCUMBENT, "a", &fresh, tokio::time::Instant::now(), true)
        .unwrap();
    drop(guard);
    old.flushed.take().unwrap().send(()).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .is_empty());
    assert_eq!(
        applied(&owner, INCUMBENT, fresh.pending.as_ref().unwrap(), true).unwrap(),
        AppliedDisposition::Active,
        "a late generation-1 receipt must not replace the successfully served generation 2"
    );
    assert_eq!(
        owner
            .module_get(INCUMBENT, "a", ConfigGetRequest::new())
            .unwrap()
            .active
            .unwrap()
            .generation,
        2
    );
}

#[tokio::test]
async fn config_delivery_hint_releases_the_gate_and_discards_a_superseded_flush_receipt() {
    let (rig, owner, handler, mut receivers) = worker_fixture();
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    let mut receiver = receivers.remove(0);
    let mut old = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let gate = owner.module_gate("a").unwrap();
    let guard = gate
        .try_lock()
        .expect("config.changed must not hold the module gate while the module writer is stalled");
    approve(&rig, &owner, 2);
    let fresh = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    let fresh = fresh.pending.unwrap();
    assert_eq!(fresh.generation, 2);
    drop(guard);
    old.flushed.take().unwrap().send(()).unwrap();
    let mut current = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&current.frame.body).unwrap()["generation"],
        2
    );
    let window = owner.delivery_window(INCUMBENT).unwrap().unwrap();
    assert_eq!(
        (window.generation, window.attempt),
        (2, fresh.attempt.unwrap())
    );
    assert!(
        !window.delivered,
        "the old hint's receipt cannot deliver the new attempt"
    );
    current.flushed.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if owner.delivery_window(INCUMBENT).unwrap().unwrap().delivered {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[derive(Default)]
struct FakeRouteAuthority {
    policies: std::sync::Arc<std::sync::Mutex<Vec<super::super::ApprovedRoutePolicy>>>,
    live_stamps: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    new_open_generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    reject: std::sync::atomic::AtomicBool,
    events: std::sync::Arc<std::sync::Mutex<Vec<(&'static str, u64)>>>,
    on_commit: Option<std::sync::Arc<dyn Fn(u64) + Send + Sync>>,
    on_restore: Option<std::sync::Arc<dyn Fn(u64) + Send + Sync>>,
}

struct FakePolicyReceipt {
    policy: std::sync::Arc<std::sync::atomic::AtomicU64>,
    previous: u64,
    generation: u64,
    events: std::sync::Arc<std::sync::Mutex<Vec<(&'static str, u64)>>>,
    on_commit: Option<std::sync::Arc<dyn Fn(u64) + Send + Sync>>,
    on_restore: Option<std::sync::Arc<dyn Fn(u64) + Send + Sync>>,
}

impl super::super::RoutePolicyReceipt for FakePolicyReceipt {
    fn commit(&mut self) {
        if let Some(check) = &self.on_commit {
            check(self.generation);
        }
        self.events
            .lock()
            .unwrap()
            .push(("commit", self.generation));
    }
    fn restore(&mut self) {
        if let Some(check) = &self.on_restore {
            check(self.generation);
        }
        self.policy
            .store(self.previous, std::sync::atomic::Ordering::SeqCst);
        self.events
            .lock()
            .unwrap()
            .push(("restore", self.generation));
    }
}

impl FakeRouteAuthority {
    fn receipt(&self, generation: u64) -> super::super::RoutePolicyInstallation {
        super::super::RoutePolicyInstallation::new(Box::new(FakePolicyReceipt {
            policy: self.new_open_generation.clone(),
            previous: self
                .new_open_generation
                .load(std::sync::atomic::Ordering::SeqCst),
            generation,
            events: self.events.clone(),
            on_commit: self.on_commit.clone(),
            on_restore: self.on_restore.clone(),
        }))
    }
}

impl super::super::RoutePolicyAuthority for FakeRouteAuthority {
    fn install_and_revoke<'a>(
        &'a self,
        policy: super::super::ApprovedRoutePolicy,
    ) -> super::super::RoutePolicyInstall<'a> {
        Box::pin(async move {
            use std::sync::atomic::Ordering;
            if self.reject.load(Ordering::SeqCst) {
                return Err(super::super::super::GateError::new(
                    "route_install_refused",
                    "fake route authority refused installation",
                ));
            }
            let receipt = self.receipt(policy.generation());
            self.live_stamps.lock().unwrap().clear();
            self.new_open_generation
                .store(policy.generation(), Ordering::SeqCst);
            self.policies.lock().unwrap().push(policy);
            Ok(receipt)
        })
    }
}

#[test]
fn config_delivery_route_obligations_are_refused_without_an_installer_even_under_stronger_transitions(
) {
    use subc_config::ApplyKind;
    for stronger in [
        None,
        Some(ApplyKind::NextSupervision),
        Some(ApplyKind::ModuleRestart),
        Some(ApplyKind::DaemonRestart),
    ] {
        let (rig, owner) = rig();
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        let kinds: Vec<_> = std::iter::once(ApplyKind::RouteRebind)
            .chain(stronger)
            .collect();
        let error = plan_kinds(&rig, &owner, &kinds).unwrap_err();
        assert_eq!(error.code, "route_policy_authority_unavailable");
        assert!(error.detail.contains("RoutePolicyAuthority is absent"));
        assert!(error.detail.contains("routes and stamps"));
        let mut state = owner.lock().unwrap();
        assert_eq!(state.store.body().unwrap().last_generation, 1);
    }
}

#[tokio::test]
async fn config_delivery_route_authority_installs_saved_approved_bytes_and_revokes_stamps_before_activation(
) {
    use std::sync::{atomic::Ordering, Arc};
    let (rig, owner) = rig();
    let authority = Arc::new(FakeRouteAuthority::default());
    authority.live_stamps.lock().unwrap().push(1);
    authority.new_open_generation.store(1, Ordering::SeqCst);
    let owner = std::sync::Arc::new(owner.with_route_policy_authority(authority.clone()));
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    let approved_bytes = std::fs::read(rig.path.join("subc.jsonc")).unwrap();
    let plan = approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    let view = pending(&owner, INCUMBENT);
    assert!(owner.ready(INCUMBENT, "a", 2).unwrap().is_none());
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::WaitingForReadiness
    );
    assert_active(&owner, INCUMBENT, 1);
    assert_eq!(authority.new_open_generation.load(Ordering::SeqCst), 1);
    std::fs::write(rig.path.join("subc.jsonc"), b"unapproved on-disk edit").unwrap();
    let gate = owner.module_gate("a").unwrap();
    let _guard = gate.lock().await;
    owner.install_approved_route_policy("a", 2).await.unwrap();
    assert!(authority.live_stamps.lock().unwrap().is_empty());
    assert_eq!(authority.new_open_generation.load(Ordering::SeqCst), 2);
    assert_active(&owner, INCUMBENT, 1);
    owner.ready(INCUMBENT, "a", 2).unwrap();
    assert_active(&owner, INCUMBENT, 2);
    let policies = authority.policies.lock().unwrap();
    assert_eq!(policies.len(), 1);
    let policy = &policies[0];
    assert_eq!((policy.module(), policy.generation()), ("a", 2));
    assert_eq!(policy.plan_digest(), &plan.digest);
    assert_eq!(
        policy.delivery().daemon.as_ref().unwrap().bytes,
        Some(approved_bytes)
    );
    assert!(policy
        .changeset()
        .fields
        .iter()
        .any(|field| field.apply == subc_config::ApplyKind::RouteRebind));
}

#[tokio::test]
async fn config_delivery_route_authority_failure_cannot_be_counted_as_ready() {
    use std::sync::{atomic::Ordering, Arc};
    let (rig, owner) = rig();
    let authority = Arc::new(FakeRouteAuthority::default());
    let owner = std::sync::Arc::new(owner.with_route_policy_authority(authority.clone()));
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    let view = pending(&owner, INCUMBENT);
    owner.ready(INCUMBENT, "a", 2).unwrap();
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::WaitingForReadiness
    );
    authority.reject.store(true, Ordering::SeqCst);
    assert_eq!(
        owner
            .install_approved_route_policy("a", 2)
            .await
            .unwrap_err()
            .code,
        "route_install_refused"
    );
    assert_active(&owner, INCUMBENT, 1);
    assert_eq!(pending(&owner, INCUMBENT).generation, 2);
}

#[test]
fn config_delivery_route_receipt_drop_restores_and_double_restore_is_a_noop_without_resurrecting_stamps(
) {
    use std::sync::atomic::Ordering;
    let authority = FakeRouteAuthority::default();
    authority.new_open_generation.store(77, Ordering::SeqCst);
    authority.live_stamps.lock().unwrap().push(77);
    {
        let _receipt = authority.receipt(2);
        authority.new_open_generation.store(2, Ordering::SeqCst);
        authority.live_stamps.lock().unwrap().clear();
    }
    assert_eq!(
        authority.new_open_generation.load(Ordering::SeqCst),
        77,
        "dropping an unresolved install restores captured pre-gate policy"
    );
    assert!(authority.live_stamps.lock().unwrap().is_empty());
    assert_eq!(*authority.events.lock().unwrap(), vec![("restore", 2)]);
    let mut receipt = authority.receipt(3);
    authority.new_open_generation.store(3, Ordering::SeqCst);
    receipt.restore();
    receipt.restore();
    drop(receipt);
    assert_eq!(authority.new_open_generation.load(Ordering::SeqCst), 77);
    assert_eq!(
        *authority.events.lock().unwrap(),
        vec![("restore", 2), ("restore", 3)]
    );
}

#[tokio::test]
async fn config_delivery_route_receipt_ack_commits_after_durable_publication_and_nack_restores_before_publication(
) {
    use std::sync::{atomic::Ordering, Arc};
    for ack in [true, false] {
        let (rig, owner) = rig();
        let mut authority = FakeRouteAuthority::default();
        authority.new_open_generation.store(1, Ordering::SeqCst);
        let store_root = rig.path.join("store");
        let read = move || {
            let mut store = ConfigStore::open_digest_only_for_tests(
                StoreRoot::from_service_definition(store_root.clone()).unwrap(),
            );
            store.body().unwrap().clone()
        };
        if ack {
            authority.on_commit = Some(Arc::new(move |generation| {
                let body = read();
                assert_eq!(
                    body.versions[0].1.active,
                    Some(generation),
                    "receipt commit must follow durable active publication"
                );
                assert!(body.versions[0].1.pending.is_none());
            }));
        } else {
            authority.on_restore = Some(Arc::new(move |generation| {
                let body = read();
                assert_eq!(
                    body.versions[0].1.pending,
                    Some(generation),
                    "policy restore must happen before publishing the failed generation"
                );
                assert!(!body.generations[&generation]
                    .generation
                    .never_redeliver
                    .contains(&"a".into()));
            }));
        }
        let authority = Arc::new(authority);
        let owner = std::sync::Arc::new(owner.with_route_policy_authority(authority.clone()));
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
        let view = pending(&owner, INCUMBENT);
        owner.install_approved_route_policy("a", 2).await.unwrap();
        owner.ready(INCUMBENT, "a", 2).unwrap();
        applied(&owner, INCUMBENT, &view, ack).unwrap();
        assert_eq!(
            authority.new_open_generation.load(Ordering::SeqCst),
            if ack { 2 } else { 1 }
        );
        assert_active(&owner, INCUMBENT, if ack { 2 } else { 1 });
        assert_eq!(
            *authority.events.lock().unwrap(),
            vec![(if ack { "commit" } else { "restore" }, 2)]
        );
    }
}

#[tokio::test]
async fn config_delivery_route_receipt_waits_for_the_ack_anchor_before_committing() {
    use std::sync::Arc;
    let rig = Rig::new();
    write(&rig, 1);
    let key = crate::config_store::TestStoreKey::default();
    let authority = Arc::new(FakeRouteAuthority::default());
    let owner = std::sync::Arc::new(
        sealed_owner(&rig, &key, "route-ack-anchor").with_route_policy_authority(authority.clone()),
    );
    rig.declare(&owner, INCUMBENT.get());
    rig.stamp(&owner);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    let view = pending(&owner, INCUMBENT);
    owner.install_approved_route_policy("a", 2).await.unwrap();
    owner.ready(INCUMBENT, "a", 2).unwrap();
    key.fail_anchor(true);
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap_err().code,
        "anchor_pending"
    );
    assert!(
        authority.events.lock().unwrap().is_empty(),
        "an unanchored ACK cannot commit the policy receipt"
    );
    assert!(owner.status().unwrap().anchor_pending);
    assert_eq!(
        owner.lock().unwrap().store.body().unwrap().versions[0]
            .1
            .active,
        Some(1)
    );
    key.fail_anchor(false);
    assert!(!owner.status().unwrap().anchor_pending);
    assert_eq!(*authority.events.lock().unwrap(), vec![("commit", 2)]);
}

#[tokio::test]
async fn config_delivery_route_receipt_is_retained_on_decline_and_restored_before_a_superseding_approval(
) {
    use std::sync::{atomic::Ordering, Arc};
    for approve in [false, true] {
        let (rig, owner) = rig();
        let authority = Arc::new(FakeRouteAuthority::default());
        authority.new_open_generation.store(1, Ordering::SeqCst);
        let owner = std::sync::Arc::new(owner.with_route_policy_authority(authority.clone()));
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
        let view = pending(&owner, INCUMBENT);
        owner.install_approved_route_policy("a", 2).await.unwrap();
        owner.ready(INCUMBENT, "a", 2).unwrap();
        write(&rig, 3);
        let next = plan_kinds_from_disk(&owner, &[subc_config::ApplyKind::RouteRebind]).unwrap();
        owner
            .desktop_view_delivered(ConnectionId::new(7), &next)
            .unwrap();
        let challenge = owner.begin_approval(&next.digest, &Rig::facts()).unwrap();
        let decision = if approve {
            super::super::super::PresenceDecision::InteractiveApproval
        } else {
            super::super::super::PresenceDecision::Declined
        };
        let result = owner.finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(decision),
            3,
        );
        if approve {
            assert_eq!(result.unwrap(), 3);
            assert_eq!(authority.new_open_generation.load(Ordering::SeqCst), 1);
            assert_eq!(*authority.events.lock().unwrap(), vec![("restore", 2)]);
            assert_eq!(pending(&owner, INCUMBENT).generation, 3);
        } else {
            assert!(result.is_err());
            assert_eq!(authority.new_open_generation.load(Ordering::SeqCst), 2);
            assert!(authority.events.lock().unwrap().is_empty());
            assert_eq!(
                applied(&owner, INCUMBENT, &view, true).unwrap(),
                AppliedDisposition::Active
            );
            assert_eq!(*authority.events.lock().unwrap(), vec![("commit", 2)]);
        }
    }
}

#[tokio::test]
async fn config_delivery_route_receipts_are_not_persisted_and_owner_drop_restores_before_reopen() {
    use std::sync::{atomic::Ordering, Arc};
    let (rig, owner) = rig();
    let authority = Arc::new(FakeRouteAuthority::default());
    authority.new_open_generation.store(1, Ordering::SeqCst);
    let owner = std::sync::Arc::new(owner.with_route_policy_authority(authority.clone()));
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    owner.install_approved_route_policy("a", 2).await.unwrap();
    assert_eq!(authority.new_open_generation.load(Ordering::SeqCst), 2);
    drop(owner);
    assert_eq!(authority.new_open_generation.load(Ordering::SeqCst), 1);
    let restarted = rig.owner("route-reopened");
    rig.declare(&restarted, INCUMBENT.get());
    let reply = restarted
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(reply.active.unwrap().generation, 1);
    assert_eq!(reply.pending.unwrap().generation, 2);
    assert!(restarted.lock().unwrap().route_policy_receipts.is_empty());
    assert!(restarted.route_policy_needed("a", 2).unwrap());
    assert!(store_bytes(&rig)
        .windows(b"receipt".len())
        .all(|window| window != b"receipt"));
}

#[cfg(unix)]
#[tokio::test]
async fn config_delivery_approved_restart_preserves_the_crash_ring_and_its_warmup_failure_spends_once(
) {
    let (rig, owner, supervisor, handler) = supervised_fixture(true);
    let module = supervisor.spawn(supervised_spec(&rig, "a")).unwrap();
    wait_for_supervised_stage(&module, "first crash raises exit fence", || {
        owner.delivery_exit_fence("a").unwrap().is_some()
    })
    .await;
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    wait_for_supervised_stage(&module, "first crash respawns generation two", || {
        module
            .status()
            .is_ok_and(|status| status.spawn_generation == 2 && status.process_alive)
    })
    .await;
    assert_eq!(module.status().unwrap().restart_count, 1);
    // A generation is published before the resumed shell records its start.
    // Let generation two begin warmup before approval replaces that child.
    wait_for_supervised_stage(&module, "second script starts before approval", || {
        std::fs::read_to_string(rig.path.join("a.launches"))
            .is_ok_and(|text| text.lines().count() == 2)
    })
    .await;
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::ModuleRestart]);
    owner.notify_delivery_changed();
    wait_for_supervised_stage(&module, "approved restart spawns generation three", || {
        module
            .status()
            .is_ok_and(|status| status.spawn_generation == 3 && status.process_alive)
    })
    .await;
    assert_eq!(
        module.status().unwrap().restart_count,
        1,
        "approval neither spends nor clears the preceding real crash"
    );
    wait_for_supervised_stage(&module, "approved script records its third start", || {
        std::fs::read_to_string(rig.path.join("a.launches"))
            .is_ok_and(|text| text.lines().count() == 3)
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(rig.path.join("a.launches"))
            .unwrap()
            .lines()
            .count(),
        3
    );
    module.config_apply_restart(3).await.unwrap();
    let pid = module.status().unwrap().pid.unwrap();
    assert!(std::process::Command::new("/bin/kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap()
        .success());
    wait_for_supervised_stage(
        &module,
        "replacement crash respawns generation four",
        || {
            module
                .status()
                .is_ok_and(|status| status.spawn_generation == 4 && status.process_alive)
        },
    )
    .await;
    assert_eq!(
        module.status().unwrap().restart_count,
        2,
        "the failed replacement spends exactly one additional entry"
    );
    assert!(owner.delivery_exit_fence("a").unwrap().is_none());
    rig.declare(&owner, 30);
    let view = owner
        .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(view.active.unwrap().generation, 1);
    assert!(view.pending.is_none());
    module.stop().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn config_delivery_swap_superseded_by_approval_keeps_the_incumbent_then_starts_one_current_command(
) {
    use crate::supervise::{
        ModuleOverlap, ModuleState, RestartPolicy, Supervisor, SupervisorHandle, SwapOutcome,
    };
    use std::sync::Arc;
    let (rig, owner, _, _) = supervised_fixture(false);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    std::fs::write(rig.path.join("a.seen"), b"keep running").unwrap();
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let handle = SupervisorHandle::new();
    // Modules register directly in this fixture. Capture their inherited
    // channels so the absent socket service does not cause launch refusal.
    handle.capture_registration_channels_for_test();
    let supervisor =
        Supervisor::new_for_test(registry.clone(), RestartPolicy::new(3, Duration::ZERO))
            .with_handle(handle.clone())
            .with_forwarding(forwarding.clone())
            .with_config_runtime(owner.clone())
            .with_drain_timeout(Duration::from_millis(20));
    let handler = Arc::new(
        crate::ControlHandler::with_forwarding(registry.clone(), forwarding.clone())
            .with_supervisor(handle)
            .with_config_runtime(owner.clone()),
    );
    let mut spec = supervised_spec(&rig, "a");
    spec.protocol = subc_control::ModuleProtocol::Subc;
    spec.overlap = ModuleOverlap::Safe;
    let module = supervisor.spawn(spec.clone()).unwrap();
    let mut manifest = subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build();
    manifest.ready = Some(true);
    registry
        .register_with_control_ops(
            manifest.clone(),
            subc_protocol::PROTOCOL_VERSION,
            INCUMBENT,
            vec!["config.review".into()],
        )
        .unwrap();
    let (sender, _incumbent_frames) = tokio::sync::mpsc::channel(8);
    forwarding
        .register_module_connection(
            INCUMBENT,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(sender),
        )
        .unwrap();
    let swap = {
        let module = module.clone();
        tokio::spawn(async move { module.swap(Some(Duration::from_secs(30))).await })
    };
    wait_for_supervised_stage(&module, "incumbent and candidate scripts start", || {
        std::fs::read_to_string(rig.path.join("a.launches"))
            .is_ok_and(|text| text.lines().count() == 2)
    })
    .await;
    manifest.ready = Some(false);
    registry
        .register_candidate_with_control_ops(
            manifest,
            subc_protocol::PROTOCOL_VERSION,
            CANDIDATE,
            vec!["config.review".into()],
        )
        .unwrap();
    let (sender, _candidate_frames) = tokio::sync::mpsc::channel(8);
    forwarding
        .register_candidate_module_connection(
            CANDIDATE,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(sender),
        )
        .unwrap();
    assert_eq!(
        forwarding
            .active_module_endpoint("a")
            .unwrap()
            .unwrap()
            .connection_id,
        INCUMBENT
    );
    let mut next = spec;
    next.program = rig.path.join("next-program");
    std::fs::copy(rig.path.join("program"), &next.program).unwrap();
    next.args[1] = rig
        .path
        .join("a.next-command.launches")
        .to_string_lossy()
        .into_owned();
    // The approved generation, not live file staging, selects the next command.
    std::fs::write(rig.path.join("subc.jsonc"), serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":next.program,"args":next.args,"overlap":"safe"}}})).unwrap()).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::ModuleRestart]);
    let gate = owner.module_gate("a").unwrap();
    let guard = gate.lock().await;
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    registry.deregister_connection(CANDIDATE).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), swap)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(outcome, SwapOutcome::SupersededByConfigRestart);
    assert_eq!(outcome.as_str(), "superseded_by_config_restart");
    assert_eq!(module.status().unwrap().swap_outcome, Some(outcome));
    assert_eq!(module.state().unwrap(), ModuleState::Running);
    assert_eq!(
        forwarding
            .active_module_endpoint("a")
            .unwrap()
            .unwrap()
            .connection_id,
        INCUMBENT
    );
    assert_eq!(module.status().unwrap().restart_count, 0);
    registry.deregister_connection(INCUMBENT).unwrap();
    drop(guard);
    wait_for_supervised_stage(&module, "approved current command starts", || {
        // Shell redirection creates the file before echo writes its record.
        std::fs::read_to_string(rig.path.join("a.next-command.launches"))
            .is_ok_and(|text| !text.is_empty())
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(rig.path.join("a.next-command.launches"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(module.status().unwrap().spawn_generation, 2);
    assert_eq!(module.status().unwrap().restart_count, 0);
    assert_eq!(
        std::fs::read_to_string(rig.path.join("a.launches"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let replacement = ConnectionId::new(30);
    registry
        .register_with_control_ops(
            subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
            subc_protocol::PROTOCOL_VERSION,
            replacement,
            vec!["config.review".into()],
        )
        .unwrap();
    let (sender, _replacement_frames) = tokio::sync::mpsc::channel(8);
    forwarding
        .register_module_connection(
            replacement,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(sender),
        )
        .unwrap();
    rig.declare(&owner, 30);
    wait_for_supervised_stage(&module, "replacement has a config health probe", || {
        owner.health_probe(replacement, "a").unwrap().is_some()
    })
    .await;
    assert_eq!(pending(&owner, replacement).generation, 2);
    registry.deregister_connection(replacement).unwrap();
    module.stop().await.unwrap();
}

#[test]
fn config_delivery_swap_never_uses_cutover_as_a_missing_start_or_daemon_boundary() {
    for kind in [
        subc_config::ApplyKind::ModuleRestart,
        subc_config::ApplyKind::NextSupervision,
        subc_config::ApplyKind::DaemonRestart,
    ] {
        let (rig, owner) = rig();
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        let spawn = owner
            .spawned(
                "a",
                tokio::time::Instant::now(),
                Duration::from_secs(120),
                Duration::from_secs(5),
            )
            .unwrap()
            .unwrap();
        candidate(&rig, &owner);
        owner.registered(CANDIDATE, &spawn).unwrap();
        approve_kinds(&rig, &owner, &[kind]);
        let result = owner.promoted(CANDIDATE, "a", tokio::time::Instant::now());
        if kind == subc_config::ApplyKind::DaemonRestart {
            assert_eq!(
                owner.requires_fresher_spawn("a", Some(&spawn)).unwrap(),
                None,
                "an approval deferred to the next daemon must not restart a running module"
            );
            assert_eq!(
                result.unwrap().unwrap().0,
                1,
                "only active per-child catch-up is eligible"
            );
            let catchup = pending(&owner, CANDIDATE);
            assert_eq!(catchup.generation, 1);
            assert_eq!(
                applied(&owner, CANDIDATE, &catchup, true).unwrap(),
                AppliedDisposition::CatchupComplete
            );
            assert_active(&owner, CANDIDATE, 1);
            assert_eq!(
                owner.lock().unwrap().store.body().unwrap().versions[0]
                    .1
                    .pending,
                Some(2)
            );
        } else {
            assert_eq!(
                owner.requires_fresher_spawn("a", Some(&spawn)).unwrap(),
                Some(2)
            );
            assert_eq!(result.unwrap_err().code, "config_gated");
            assert_active(&owner, INCUMBENT, 1);
        }
    }
}

#[tokio::test]
async fn config_delivery_route_policy_install_does_not_reuse_pre_install_health() {
    let (rig, owner) = rig();
    let owner = std::sync::Arc::new(
        owner.with_route_policy_authority(std::sync::Arc::new(FakeRouteAuthority::default())),
    );
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    let view = pending(&owner, INCUMBENT);
    owner.ready(INCUMBENT, "a", 2).unwrap();
    applied(&owner, INCUMBENT, &view, true).unwrap();
    owner.install_approved_route_policy("a", 2).await.unwrap();
    assert_active(&owner, INCUMBENT, 1);
}

#[tokio::test]
async fn config_delivery_route_health_reply_started_before_install_does_not_count_but_a_fresh_probe_does(
) {
    let (rig, owner) = rig();
    let owner = std::sync::Arc::new(
        owner.with_route_policy_authority(std::sync::Arc::new(FakeRouteAuthority::default())),
    );
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    let view = pending(&owner, INCUMBENT);
    applied(&owner, INCUMBENT, &view, true).unwrap();
    let in_flight = owner.health_probe(INCUMBENT, "a").unwrap().unwrap();
    owner.install_approved_route_policy("a", 2).await.unwrap();
    assert_eq!(owner.health_result(in_flight, true).unwrap(), None);
    assert_active(&owner, INCUMBENT, 1);
    let fresh = owner.health_probe(INCUMBENT, "a").unwrap().unwrap();
    assert_eq!(
        owner.health_result(fresh, true).unwrap(),
        Some(AppliedDisposition::Active)
    );
    assert_active(&owner, INCUMBENT, 2);
}

#[tokio::test]
async fn config_delivery_route_unhealthy_fresh_probe_restores_policy_through_the_delivery_failure_path(
) {
    use std::sync::{atomic::Ordering, Arc};
    let (rig, owner) = rig();
    let authority = Arc::new(FakeRouteAuthority::default());
    authority.new_open_generation.store(1, Ordering::SeqCst);
    let owner = std::sync::Arc::new(owner.with_route_policy_authority(authority.clone()));
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    pending(&owner, INCUMBENT);
    let stale = owner.health_probe(INCUMBENT, "a").unwrap().unwrap();
    owner.install_approved_route_policy("a", 2).await.unwrap();
    assert_eq!(owner.health_result(stale, false).unwrap(), None);
    let fresh = owner.health_probe(INCUMBENT, "a").unwrap().unwrap();
    assert!(matches!(
        owner.health_result(fresh.clone(), false).unwrap(),
        Some(AppliedDisposition::Failed {
            generation: 2,
            restore: Some(1),
            ..
        })
    ));
    assert_eq!(authority.new_open_generation.load(Ordering::SeqCst), 1);
    assert_active(&owner, INCUMBENT, 1);
    assert_eq!(owner.health_result(fresh, true).unwrap(), None);
    assert_eq!(owner.delivery_take_recoveries().unwrap().len(), 1);
    assert!(owner.lock().unwrap().store.body().unwrap().generations[&2]
        .generation
        .never_redeliver
        .contains(&"a".into()));
}

#[cfg(unix)]
#[tokio::test]
async fn config_delivery_route_worker_requests_immediate_health_and_uses_delivery_failure_for_unhealthy_or_timeout(
) {
    use crate::supervise::{HealthConfig, RestartPolicy, Supervisor, SupervisorHandle};
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{atomic::Ordering, Arc},
    };
    use subc_protocol::session::{HealthStatus, ModuleControlResponse};
    for answer in [Some(HealthStatus::Ok), Some(HealthStatus::Failing), None] {
        let rig = Rig::new();
        std::fs::write(
            rig.path.join("program"),
            b"#!/bin/sh\nwhile :; do /bin/sleep 600; done\n",
        )
        .unwrap();
        std::fs::set_permissions(
            rig.path.join("program"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        write(&rig, 1);
        let authority = Arc::new(FakeRouteAuthority::default());
        authority.new_open_generation.store(1, Ordering::SeqCst);
        let owner = Arc::new(
            rig.owner("post-policy-probe")
                .with_route_policy_authority(authority.clone()),
        );
        rig.declare(&owner, INCUMBENT.get());
        rig.stamp(&owner);
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        let registry = Arc::new(crate::Registry::default());
        let forwarding = Arc::new(crate::ForwardingTable::default());
        let handle = SupervisorHandle::new();
        let health = HealthConfig {
            cadence: Duration::from_secs(3600),
            ..HealthConfig::default()
        };
        let supervisor =
            Supervisor::new_for_test(registry.clone(), RestartPolicy::new(3, Duration::ZERO))
                .with_handle(handle.clone())
                .with_forwarding(forwarding.clone())
                .with_health_config(health)
                .with_config_runtime(owner.clone())
                .with_drain_timeout(Duration::ZERO);
        let mut spec = supervised_spec(&rig, "a");
        spec.protocol = subc_control::ModuleProtocol::Subc;
        let module = supervisor.spawn(spec).unwrap();
        registry
            .register_with_control_ops(
                subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
                subc_protocol::PROTOCOL_VERSION,
                INCUMBENT,
                vec!["config.review".into(), "health.check".into()],
            )
            .unwrap();
        let (sender, mut frames) = tokio::sync::mpsc::channel(8);
        forwarding
            .register_module_connection(
                INCUMBENT,
                "a".into(),
                subc_protocol::PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                crate::FrameSink::new(sender),
            )
            .unwrap();
        let handler = Arc::new(
            crate::ControlHandler::with_forwarding(registry.clone(), forwarding.clone())
                .with_supervisor(handle)
                .with_config_runtime(owner.clone()),
        );
        approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
        let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
        let frame = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut frame = frames.recv().await.unwrap();
                if let Some(flushed) = frame.flushed.take() {
                    flushed.send(()).unwrap();
                }
                if serde_json::from_slice::<serde_json::Value>(&frame.body)
                    .is_ok_and(|body| body["op"] == "health.check")
                {
                    break frame;
                }
            }
        })
        .await
        .expect("the installation must request health immediately, not at its hour-long cadence");
        assert_eq!(authority.new_open_generation.load(Ordering::SeqCst), 2);
        assert_active(&owner, INCUMBENT, 1);
        let view = pending(&owner, INCUMBENT);
        if answer == Some(HealthStatus::Ok) {
            assert_eq!(
                applied(&owner, INCUMBENT, &view, true).unwrap(),
                AppliedDisposition::WaitingForReadiness
            );
        }
        if let Some(status) = answer {
            forwarding
                .complete_module_control_rpc(
                    INCUMBENT,
                    frame.header.corr,
                    Some("health.check"),
                    crate::forwarding::ModuleControlRpcOutcome::Response(
                        ModuleControlResponse::HealthCheck {
                            status,
                            detail: None,
                            metrics: None,
                        },
                    ),
                )
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let body = owner.lock().unwrap().store.body().unwrap().clone();
                if if answer == Some(HealthStatus::Ok) {
                    body.versions[0].1.active == Some(2)
                } else {
                    body.generations[&2]
                        .generation
                        .never_redeliver
                        .contains(&"a".into())
                } {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the fresh health result must settle configuration delivery");
        assert_active(
            &owner,
            INCUMBENT,
            if answer == Some(HealthStatus::Ok) {
                2
            } else {
                1
            },
        );
        assert_eq!(
            authority.new_open_generation.load(Ordering::SeqCst),
            if answer == Some(HealthStatus::Ok) {
                2
            } else {
                1
            }
        );
        if answer != Some(HealthStatus::Ok) {
            wait_for_delivery_condition(|| {
                module
                    .status()
                    .is_ok_and(|status| status.restart_count == 1)
            })
            .await;
            assert_eq!(
                module.status().unwrap().restart_count,
                1,
                "config failure spends once, not a second time through health supervision"
            );
        } else {
            assert_eq!(module.status().unwrap().restart_count, 0);
        }
        registry.deregister_connection(INCUMBENT).unwrap();
        module.stop().await.unwrap();
    }
}

async fn wait_for_blocking_delivery(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "blocking delivery completion was not observed"
        );
        // Keep the paused clock from automatically advancing ACK deadlines
        // while an operating-system thread completes its store operation.
        tokio::task::yield_now().await;
    }
}

fn hold_test_clock() -> WorkerGuard {
    // Blocking jobs are invisible to Tokio's idle-clock detection. Only the
    // test's explicit advances, not disk/keychain scheduling, move time here.
    WorkerGuard(tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    }))
}

#[cfg(unix)]
#[tokio::test]
async fn config_delivery_swap_candidate_exit_requeues_busy_approved_restart_exactly_once() {
    restart_after_busy_swap_warmup(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn config_delivery_swap_owner_release_supersedes_retained_restart_exactly_once() {
    restart_after_busy_swap_warmup(false).await;
}

#[cfg(unix)]
async fn restart_after_busy_swap_warmup(candidate_exits_first: bool) {
    use crate::supervise::{ModuleOverlap, RestartPolicy, Supervisor, SupervisorHandle};
    use std::{os::unix::fs::PermissionsExt, sync::Arc};
    let rig = Rig::new();
    write(&rig, 1);
    std::fs::write(
        rig.path.join("program"),
        b"#!/bin/sh\nprintf '%s\\n' \"$$\" >> \"$2\"\nwhile :; do /bin/sleep 600; done\n",
    )
    .unwrap();
    std::fs::set_permissions(
        rig.path.join("program"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let mut spec = supervised_spec(&rig, "a");
    spec.protocol = subc_control::ModuleProtocol::Subc;
    spec.overlap = ModuleOverlap::Safe;
    std::fs::write(rig.path.join("subc.jsonc"),serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":spec.program,"args":spec.args,"protocol":"subc","overlap":"safe"}}})).unwrap()).unwrap();
    let owner = Arc::new(rig.owner("restart-after-candidate-exit"));
    rig.declare(&owner, INCUMBENT.get());
    rig.stamp(&owner);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let handle = SupervisorHandle::new();
    // Modules register directly in this fixture. Capture their inherited
    // channels so the absent socket service does not cause launch refusal.
    handle.capture_registration_channels_for_test();
    let supervisor =
        Supervisor::new_for_test(registry.clone(), RestartPolicy::new(3, Duration::ZERO))
            .with_handle(handle.clone())
            .with_forwarding(forwarding.clone())
            .with_config_runtime(owner.clone())
            .with_drain_timeout(Duration::from_millis(20));
    let handler = Arc::new(
        crate::ControlHandler::with_forwarding(registry.clone(), forwarding.clone())
            .with_supervisor(handle)
            .with_config_runtime(owner.clone()),
    );
    let module = supervisor.spawn(spec.clone()).unwrap();
    let mut manifest = subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build();
    manifest.ready = Some(true);
    registry
        .register_with_control_ops(
            manifest.clone(),
            subc_protocol::PROTOCOL_VERSION,
            INCUMBENT,
            vec!["config.review".into()],
        )
        .unwrap();
    let (sender, _incumbent_frames) = tokio::sync::mpsc::channel(8);
    forwarding
        .register_module_connection(
            INCUMBENT,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(sender),
        )
        .unwrap();
    let swap = {
        let module = module.clone();
        tokio::spawn(async move { module.swap(Some(Duration::from_secs(30))).await })
    };
    wait_for_delivery_condition(|| {
        std::fs::read_to_string(rig.path.join("a.launches"))
            .is_ok_and(|text| text.lines().count() == 2)
    })
    .await;
    manifest.ready = Some(false);
    registry
        .register_candidate_with_control_ops(
            manifest,
            subc_protocol::PROTOCOL_VERSION,
            CANDIDATE,
            vec!["config.review".into()],
        )
        .unwrap();
    let (sender, _candidate_frames) = tokio::sync::mpsc::channel(8);
    forwarding
        .register_candidate_module_connection(
            CANDIDATE,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(sender),
        )
        .unwrap();
    let mut next = spec;
    next.program = rig.path.join("next-program");
    std::fs::copy(rig.path.join("program"), &next.program).unwrap();
    next.args[1] = rig
        .path
        .join("a.current.launches")
        .to_string_lossy()
        .into_owned();
    std::fs::write(rig.path.join("subc.jsonc"),serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":next.program,"args":next.args,"protocol":"subc","overlap":"safe"}}})).unwrap()).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::ModuleRestart]);
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
    *owner.warm_restart_busy.lock().unwrap() = Some(busy_tx);
    *owner.warm_restart_pause.lock().unwrap() =
        Some(crate::config_gate::runtime::WarmRestartPause {
            reached: reached_tx,
            resume: resume_rx,
        });
    let _worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
    tokio::time::timeout(Duration::from_secs(5), reached_rx)
        .await
        .unwrap()
        .unwrap();
    let guard = owner.lock().unwrap();
    resume_tx.send(()).unwrap();
    // Observe the actual busy freshness check before choosing which event wins.
    tokio::time::timeout(Duration::from_secs(5), busy_rx)
        .await
        .unwrap()
        .unwrap();
    if candidate_exits_first {
        let candidate_pid = std::fs::read_to_string(rig.path.join("a.launches"))
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(candidate_pid).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
        // Keep the configuration owner locked until warm-up observes the
        // candidate's exit, not just until the OS reports it.
        let outcome = tokio::time::timeout(Duration::from_secs(5), swap)
            .await
            .unwrap()
            .unwrap();
        drop(guard);
        assert!(
            matches!(
                outcome,
                Err(crate::supervise::SuperviseError::SwapFailed {
                    arm: crate::supervise::SwapFailureArm::CandidateExited,
                    ..
                })
            ),
            "expected candidate exit while the configuration owner was busy: {outcome:?}"
        );
    } else {
        // The live candidate cannot exit first. Let the next freshness check
        // replace the retained command with one superseding restart instead.
        drop(guard);
        let outcome = tokio::time::timeout(Duration::from_secs(5), swap)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome,
            crate::supervise::SwapOutcome::SupersededByConfigRestart
        );
    }
    registry.deregister_connection(INCUMBENT).unwrap();
    let restarted = tokio::time::timeout(Duration::from_secs(5), async {
        while !rig.path.join("a.current.launches").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    // The worker sends (module a, generation 2) only once. Candidate exit must
    // retain that command; supersession must replace it, never duplicate it.
    if restarted {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let launches = std::fs::read_to_string(rig.path.join("a.current.launches"))
        .unwrap_or_default()
        .lines()
        .count();
    let status = module.status().unwrap();
    module.stop().await.unwrap();
    assert!(
        restarted,
        "candidate exit lost the worker's one approved restart"
    );
    assert_eq!(launches, 1);
    assert_eq!(status.spawn_generation, 2);
    assert_eq!(status.restart_count, 0);
}
