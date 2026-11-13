use super::*;
use crate::spawn_authority::{entry_digest, SpawnAuthorityRecord};
use subc_protocol::{peer::CallerOrigin, provider_declarations::ProviderDeclarations};

fn requested() -> ProviderDeclarations {
    serde_json::from_value(
        json!({"provider-a":{"items":["one"]}, "provider-b":{"operation":"read"}}),
    )
    .unwrap()
}

fn spawn() -> std::sync::Arc<SpawnAuthorityRecord> {
    let record = SpawnAuthorityRecord::new("incarnation", 4, "a");
    record.initialize_origin(CallerOrigin::Development, Default::default(), Some(false));
    record
}

#[test]
fn origins_program_approval_card_shows_other_without_evidence() {
    let before = snapshot();
    let mut after = before.clone();
    after
        .inputs
        .insert(program("a"), captured(json!("program-v2")));
    let changeset = crate::config_gate::runtime::daemon_review(&before, &after).unwrap();
    assert_eq!(changeset.launch_manifests["a"]["origin"], "other");
}

#[test]
fn origins_widening_one_card_exact_entries_and_origin_then_persists() {
    let mut rig = Rig::new();
    rig.stamp();
    let pointers = serde_json::to_value(&rig.store.body().unwrap().versions).unwrap();
    let record = spawn();
    let differing = record.initialize_declarations(requested());
    assert!(record.capture().unwrap().approved.entries.is_empty());
    assert_eq!(differing, requested());
    assert!(
        record.initialize_declarations(requested()).is_empty(),
        "only the first HELLO files a card"
    );
    let view = rig
        .gate
        .plan_development_widening(&mut rig.store, &record, differing, &rig.current)
        .unwrap();
    let facts = facts(false);
    let challenge = rig
        .gate
        .begin_approval(&view.digest, &mut rig.store, &rig.current, &facts)
        .unwrap();
    let ApprovalChallenge::Phone {
        attempt,
        connection,
        request,
        ..
    } = challenge
    else {
        panic!("expected full card")
    };
    assert!(request.is_consistent());
    assert_eq!(
        request.plan.changeset.launch_manifests["a"]["origin"],
        "development"
    );
    assert_eq!(
        request.plan.changeset.launch_manifests["a"]["provider_declarations"],
        serde_json::to_value(requested()).unwrap()
    );
    rig.gate.bind_phone_rpc(attempt, connection, 22).unwrap();
    rig.gate
        .finish_approval(
            attempt,
            ApprovalReply::Phone {
                connection,
                correlation: 22,
                response: Some(ConsentResponse::new(&request, ConsentDecision::Approved)),
            },
            &mut rig.store,
            &rig.current,
            &facts,
            1,
        )
        .unwrap();
    let digests = rig.store.body().unwrap().approved_declarations["a"].clone();
    assert_eq!(
        digests["provider-a"],
        entry_digest("provider-a", &requested()["provider-a"]).unwrap()
    );
    assert_eq!(digests.len(), 2);
    assert_eq!(
        serde_json::to_value(&rig.store.body().unwrap().versions).unwrap(),
        pointers
    );
    let mut reopened = ConfigStore::open_digest_only_for_tests(
        StoreRoot::from_service_definition(rig.path.clone()).unwrap(),
    );
    assert_eq!(reopened.body().unwrap().approved_declarations["a"], digests);
    let next = SpawnAuthorityRecord::new("incarnation", 5, "a");
    next.initialize_origin(CallerOrigin::Development, digests, Some(false));
    assert!(next.initialize_declarations(requested()).is_empty());
    assert_eq!(next.capture().unwrap().approved.entries, requested());
}

#[test]
fn origins_widening_decline_leaves_differing_key_empty_and_keeps_equal_entry() {
    let mut rig = Rig::new();
    rig.stamp();
    let record = spawn();
    let mut digests = BTreeMap::new();
    digests.insert(
        "provider-a".into(),
        entry_digest("provider-a", &requested()["provider-a"]).unwrap(),
    );
    record.initialize_origin(CallerOrigin::Development, digests, Some(false));
    let differing = record.initialize_declarations(requested());
    assert_eq!(differing.keys().collect::<Vec<_>>(), vec!["provider-b"]);
    let view = rig
        .gate
        .plan_development_widening(&mut rig.store, &record, differing, &rig.current)
        .unwrap();
    let facts = facts(false);
    let challenge = rig
        .gate
        .begin_approval(&view.digest, &mut rig.store, &rig.current, &facts)
        .unwrap();
    let ApprovalChallenge::Phone {
        attempt,
        connection,
        request,
        ..
    } = challenge
    else {
        panic!("expected full card")
    };
    rig.gate.bind_phone_rpc(attempt, connection, 22).unwrap();
    assert_eq!(
        rig.gate
            .finish_approval(
                attempt,
                ApprovalReply::Phone {
                    connection,
                    correlation: 22,
                    response: Some(ConsentResponse::new(&request, ConsentDecision::Declined))
                },
                &mut rig.store,
                &rig.current,
                &facts,
                1
            )
            .unwrap_err()
            .code,
        "approval_declined"
    );
    assert!(rig.store.body().unwrap().approved_declarations.is_empty());
    assert!(record
        .capture()
        .unwrap()
        .approved
        .entries
        .contains_key("provider-a"));
    assert!(!record
        .capture()
        .unwrap()
        .approved
        .entries
        .contains_key("provider-b"));
}
