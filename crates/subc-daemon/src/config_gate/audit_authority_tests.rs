use super::*;

fn stamp_attempt(owner: &RuntimeOwner) -> (u64, subc_config::Digest, ApprovalFacts) {
    let prepared = owner.prepare(PlanKind::Stamp).unwrap();
    let plan = owner.finish_plan(prepared, daemon_review).unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let facts = Rig::facts();
    let attempt = owner
        .begin_approval(&plan.digest, &facts)
        .unwrap()
        .attempt();
    (attempt, plan.digest, facts)
}

fn finish_stamp(
    owner: &RuntimeOwner,
    attempt: u64,
    facts: &ApprovalFacts,
    decision: crate::config_gate::PresenceDecision,
) -> Result<u64> {
    owner.finish_approval(attempt, facts, ApprovalReply::Presence(decision), 2)
}

fn tamper(rig: &Rig, owner: &RuntimeOwner) {
    std::fs::write(
        rig.path.join("store").join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(owner.check_recovery().unwrap());
}

fn sealed_owner(rig: &Rig, key: crate::config_store::TestStoreKey) -> RuntimeOwner {
    RuntimeOwner::new(
        "authority-commit".into(),
        ConfigStore::open_sealed_for_tests(
            StoreRoot::from_service_definition(rig.path.join("store")).unwrap(),
            key,
        ),
        ConfigCapturer {
            daemon_path: rig.path.join("subc.jsonc"),
            config_home: rig.path.clone(),
            projects: std::sync::Arc::new(crate::config_gate::capture::UnavailableProjectRegistry),
        },
    )
}

#[test]
fn authority_commit_declined_identical_restamp_keeps_token_and_exit() {
    let rig = Rig::new();
    let owner = rig.owner("declined-authority");
    assert_eq!(rig.stamp(&owner), 1);
    owner.complete_authority_exit(1).unwrap();
    let old_digest = owner
        .lock()
        .unwrap()
        .store
        .loaded_admission_state()
        .1
        .generations[&1]
        .generation
        .plan_digest
        .clone();
    tamper(&rig, &owner);
    let (attempt, digest, facts) = stamp_attempt(&owner);
    assert_eq!(digest, old_digest);
    let before = owner.lock().unwrap().authority_incarnation.clone();
    assert_eq!(owner.lock().unwrap().authority_exit, None);
    assert_eq!(
        finish_stamp(
            &owner,
            attempt,
            &facts,
            crate::config_gate::PresenceDecision::Declined
        )
        .unwrap_err()
        .code,
        "approval_declined"
    );
    let state = owner.lock().unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&before, &state.authority_incarnation),
        "a declined attempt cannot rotate authority"
    );
    assert_eq!(
        state.authority_exit, None,
        "a declined attempt cannot re-arm an exit"
    );
    assert!(!state.recovery_exit_hold);
    assert!(!state.stamp_channel_cutover);
}

#[test]
fn authority_commit_anchor_error_still_rotates_rebuilt_authority() {
    let rig = Rig::new();
    let key = crate::config_store::TestStoreKey::default();
    let owner = sealed_owner(&rig, key.clone());
    assert_eq!(rig.stamp(&owner), 1);
    owner.complete_authority_exit(1).unwrap();
    tamper(&rig, &owner);
    let before = owner.lock().unwrap().authority_incarnation.clone();
    let sequence = owner.lock().unwrap().store.publication_sequence();
    let (attempt, _, facts) = stamp_attempt(&owner);
    key.fail_anchor(true);
    let error = finish_stamp(
        &owner,
        attempt,
        &facts,
        crate::config_gate::PresenceDecision::InteractiveApproval,
    )
    .unwrap_err();
    assert_eq!(error.code, "anchor_pending", "{error}");
    let state = owner.lock().unwrap();
    assert_eq!(state.store.publication_sequence(), sequence + 1);
    assert!(!std::sync::Arc::ptr_eq(
        &before,
        &state.authority_incarnation
    ));
    assert_eq!(state.authority_exit, Some(1));
    assert!(state.recovery_exit_hold);
}

#[test]
fn authority_commit_failed_duplicate_keeps_pending_exit_and_token() {
    let rig = Rig::new();
    let key = crate::config_store::TestStoreKey::default();
    let owner = sealed_owner(&rig, key.clone());
    key.fail_anchor(true);
    let (initial, _, initial_facts) = stamp_attempt(&owner);
    assert_eq!(
        finish_stamp(
            &owner,
            initial,
            &initial_facts,
            crate::config_gate::PresenceDecision::InteractiveApproval
        )
        .unwrap_err()
        .code,
        "anchor_pending"
    );
    tamper(&rig, &owner);
    let (attempt, digest, facts) = stamp_attempt(&owner);
    let duplicate = owner.begin_approval(&digest, &facts).unwrap().attempt();
    assert!(finish_stamp(
        &owner,
        attempt,
        &facts,
        crate::config_gate::PresenceDecision::InteractiveApproval
    )
    .is_err());
    let before = owner.lock().unwrap().authority_incarnation.clone();
    let sequence = owner.lock().unwrap().store.publication_sequence();
    assert_eq!(owner.lock().unwrap().authority_exit, Some(1));
    let error = finish_stamp(
        &owner,
        duplicate,
        &facts,
        crate::config_gate::PresenceDecision::InteractiveApproval,
    )
    .unwrap_err();
    assert_eq!(error.code, "plan_stale", "{error}");
    let state = owner.lock().unwrap();
    assert_eq!(state.store.publication_sequence(), sequence);
    assert!(
        std::sync::Arc::ptr_eq(&before, &state.authority_incarnation),
        "a failed duplicate cannot rotate already-committed authority"
    );
    assert_eq!(state.authority_exit, Some(1));
    assert!(state.recovery_exit_hold);
}
