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
    let (rig, key, owner) = nack_while_anchor_pending();
    drop(owner);
    let restarted = sealed_owner(&rig, &key, "before-anchor-retry");
    rig.declare(&restarted, 30);
    assert_eq!(
        restarted
            .module_get(ConnectionId::new(30), "a", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "anchor_pending"
    );
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
    assert_eq!(
        applied(&restarted, ConnectionId::new(30), &view, false).unwrap(),
        AppliedDisposition::Failed {
            generation: 2,
            restore: Some(1),
            keep_running: false
        }
    );
    drop(restarted);
    let final_owner = sealed_owner(&rig, &key, "after-durable-nack");
    rig.declare(&final_owner, 40);
    assert!(final_owner
        .module_get(ConnectionId::new(40), "a", ConfigGetRequest::new())
        .unwrap()
        .pending
        .is_none());
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
    write(rig, 2);
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), |old, new| {
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
            2,
        )
        .unwrap();
    plan
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

fn mixed_waits_for_spawn(kinds: &[subc_config::ApplyKind], expected: &str) {
    let (rig, owner) = rig();
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

#[test]
fn config_delivery_mixed_next_supervision_and_route_rebind_wait_for_spawn() {
    mixed_waits_for_spawn(
        &[
            subc_config::ApplyKind::RouteRebind,
            subc_config::ApplyKind::NextSupervision,
        ],
        "next_supervision",
    );
}

#[test]
fn config_delivery_mixed_module_restart_and_next_supervision_wait_for_spawn() {
    mixed_waits_for_spawn(
        &[
            subc_config::ApplyKind::NextSupervision,
            subc_config::ApplyKind::ModuleRestart,
        ],
        "module_restart",
    );
}

#[test]
fn config_delivery_mixed_live_and_next_supervision_defer_live_fields_in_plan_and_activation() {
    mixed_waits_for_spawn(
        &[
            subc_config::ApplyKind::Live,
            subc_config::ApplyKind::NextSupervision,
        ],
        "next_supervision",
    );
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

#[test]
fn config_delivery_mixed_route_rebind_and_live_require_route_readiness_before_activation() {
    let (rig, owner) = rig();
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
    assert_eq!(
        owner.ready(INCUMBENT, "a", 2).unwrap(),
        Some(AppliedDisposition::Active)
    );
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
