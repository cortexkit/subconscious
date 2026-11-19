use super::*;
use subc_protocol::config::{ConfigAppliedRequest, ConfigApplyResult};

fn approved() -> (Rig, RuntimeOwner) {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"limit":2,"credential":{"vault_ref":"approved"}}"#,
    )
    .unwrap();
    let owner = rig.owner("declaration-fallback");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    owner
        .lock()
        .unwrap()
        .store
        .record_delivery(
            &crate::config_store::Tuple {
                module: "a".into(),
                project: None,
            },
            1,
            true,
        )
        .unwrap();
    (rig, owner)
}

fn changed(rig: &Rig, owner: &RuntimeOwner) {
    let mut declaration = rig.declaration.clone();
    declaration.schema["properties"]["new_setting"] =
        json!({"type":"integer","description":"New setting","default":7});
    owner
        .declare(ConnectionId::new(20), "a", declaration, true)
        .unwrap();
}

#[test]
fn config_declaration_fallback_serves_exact_active_not_edited_files() {
    let (rig, owner) = approved();
    let original = owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap()
        .active
        .unwrap();
    changed(&rig, &owner);
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), b"{\"limit\":99}").unwrap();
    let reply = owner
        .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(reply.declaration_pending.unwrap().plan_digest, None);
    assert!(reply.pending.is_none());
    let mut active = reply.active.unwrap();
    assert!(active.attempt.take().is_some());
    assert_eq!(active, original);
    assert!(active.effective.get("new_setting").is_none());
}

#[test]
fn config_declaration_fallback_names_only_plan_carrying_connection_declaration() {
    let (rig, owner) = approved();
    let old = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    changed(&rig, &owner);
    let reply = owner
        .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(reply.declaration_pending.unwrap().plan_digest, None);
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    assert_ne!(old.digest, plan.digest);
    let reply = owner
        .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(
        reply.declaration_pending.unwrap().plan_digest,
        Some(plan.digest)
    );
}

#[test]
fn config_declaration_fallback_without_active_is_refused() {
    let rig = Rig::new();
    let owner = rig.owner("no-active");
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"approved"}}"#,
    )
    .unwrap();
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    changed(&rig, &owner);
    assert_eq!(
        owner
            .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "config_gated"
    );
    owner
        .declare(
            ConnectionId::new(30),
            "fresh",
            rig.declaration.clone(),
            true,
        )
        .unwrap();
    assert_eq!(
        owner
            .module_get(ConnectionId::new(30), "fresh", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "config_gated"
    );
}

#[test]
fn config_declaration_fallback_matching_declaration_is_unchanged() {
    let (_rig, owner) = approved();
    let reply = owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap();
    assert!(reply.declaration_pending.is_none());
    assert!(reply.pending.is_none());
    assert!(reply.active.unwrap().attempt.is_none());
}

#[tokio::test]
async fn config_declaration_fallback_ack_is_catchup_and_status_reports_running() {
    let (rig, owner) = approved();
    changed(&rig, &owner);
    let reply = owner
        .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
        .unwrap();
    let view = reply.active.as_ref().unwrap();
    let revision = owner.lock().unwrap().store.status().revision;
    let request = ConfigAppliedRequest::new(
        view.generation,
        view.attempt.unwrap(),
        ConfigApplyResult::Ack,
        vec![],
    );
    assert_eq!(
        owner
            .module_applied(ConnectionId::new(20), "a", request.clone())
            .unwrap(),
        delivery::AppliedDisposition::Ignored
    );
    owner
        .module_view_delivered(
            ConnectionId::new(20),
            "a",
            &reply,
            tokio::time::Instant::now(),
            true,
        )
        .unwrap();
    assert_eq!(
        owner
            .module_applied(ConnectionId::new(20), "a", request)
            .unwrap(),
        delivery::AppliedDisposition::CatchupComplete
    );
    assert_eq!(owner.lock().unwrap().store.status().revision, revision);
    let caught_up = owner
        .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
        .unwrap();
    assert!(caught_up.active.unwrap().attempt.is_none());
    assert!(owner
        .delivery_window(ConnectionId::new(20))
        .unwrap()
        .is_none());
    let status = owner.status().unwrap();
    assert_eq!(status.state, "stamped");
    assert!(status.declaration_pending.contains_key("a"));
    assert!(!status.gated_modules.contains(&"a".into()));
    assert!(status
        .warnings
        .iter()
        .any(|warning| warning.contains("running last approved settings (generation 1)")));
    let polled = owner.polled_snapshot().unwrap();
    assert!(polled
        .projection
        .status
        .declaration_pending
        .contains_key("a"));
    match owner.settings_modules(BTreeSet::new()).unwrap() {
        subc_control::config::ConfigControlResponse::Modules { modules, .. } => {
            let row = modules.iter().find(|row| row.module_id == "a").unwrap();
            assert!(row.declaration_pending.is_some());
            assert_eq!(row.active_generation, Some(1));
            assert_eq!(row.status, "active");
        }
        _ => panic!("expected settings modules"),
    }
}

fn approve(owner: &RuntimeOwner) -> u64 {
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
            1,
        )
        .unwrap()
}

#[tokio::test]
async fn config_declaration_fallback_withholds_old_pending_then_activates_new_approval() {
    let (rig, owner) = approved();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"limit":3,"credential":{"vault_ref":"approved"}}"#,
    )
    .unwrap();
    assert_eq!(approve(&owner), 2);
    changed(&rig, &owner);
    let fallback = owner
        .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(fallback.active.as_ref().unwrap().generation, 1);
    assert!(fallback.pending.is_none());
    owner
        .module_view_delivered(
            ConnectionId::new(20),
            "a",
            &fallback,
            tokio::time::Instant::now(),
            true,
        )
        .unwrap();
    let old_ack = ConfigAppliedRequest::new(
        1,
        fallback.active.unwrap().attempt.unwrap(),
        ConfigApplyResult::Ack,
        vec![],
    );
    assert_eq!(
        owner
            .module_applied(ConnectionId::new(20), "a", old_ack.clone())
            .unwrap(),
        delivery::AppliedDisposition::CatchupComplete
    );
    let generation = approve(&owner);
    assert_eq!(
        owner
            .module_applied(ConnectionId::new(20), "a", old_ack)
            .unwrap(),
        delivery::AppliedDisposition::Ignored
    );
    let reply = owner
        .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
        .unwrap();
    assert!(reply.declaration_pending.is_none());
    let pending = reply.pending.as_ref().unwrap();
    assert_eq!(pending.generation, generation);
    owner
        .module_view_delivered(
            ConnectionId::new(20),
            "a",
            &reply,
            tokio::time::Instant::now(),
            true,
        )
        .unwrap();
    assert_eq!(
        owner
            .module_applied(
                ConnectionId::new(20),
                "a",
                ConfigAppliedRequest::new(
                    generation,
                    pending.attempt.unwrap(),
                    ConfigApplyResult::Ack,
                    vec![]
                )
            )
            .unwrap(),
        delivery::AppliedDisposition::Active
    );
    assert!(!owner
        .status()
        .unwrap()
        .declaration_pending
        .contains_key("a"));
}

#[test]
fn rollback_requires_pending_declaration_match_for_delivery() {
    let (rig, owner) = approved();
    changed(&rig, &owner);
    let generation = approve(&owner);
    let reply = owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap();
    assert!(
        reply.pending.is_none(),
        "served mismatched generation {generation}"
    );
    assert!(reply.declaration_pending.is_some());
    let active = reply.active.unwrap();
    assert_eq!(active.generation, 1);
    assert!(active.effective.get("new_setting").is_none());
}

#[tokio::test]
async fn rollback_requires_the_admitted_build_to_match_the_target() {
    let (rig, owner) = approved();
    changed(&rig, &owner);
    let generation = approve(&owner);
    let connection = ConnectionId::new(20);
    let reply = owner
        .module_get(connection, "a", ConfigGetRequest::new())
        .unwrap();
    let pending = reply.pending.as_ref().unwrap();
    owner
        .module_view_delivered(connection, "a", &reply, tokio::time::Instant::now(), true)
        .unwrap();
    assert_eq!(
        owner
            .module_applied(
                connection,
                "a",
                ConfigAppliedRequest::new(
                    generation,
                    pending.attempt.unwrap(),
                    ConfigApplyResult::Ack,
                    vec![]
                ),
            )
            .unwrap(),
        delivery::AppliedDisposition::Active,
    );

    let error = match owner.prepare(PlanKind::Rollback(1)) {
        Ok(_) => panic!("rollback was planned for a different build of a"),
        Err(error) => error,
    };
    assert_eq!(error.code, "rollback_declaration_mismatch");
    assert!(
        error.detail.contains("different build of a"),
        "{}",
        error.detail
    );
    assert!(error.detail.contains("ordinary edit"), "{}", error.detail);
    let reply = owner
        .module_get(connection, "a", ConfigGetRequest::new())
        .unwrap();
    assert!(reply.pending.is_none());
    assert_eq!(reply.active.unwrap().generation, generation);
}
