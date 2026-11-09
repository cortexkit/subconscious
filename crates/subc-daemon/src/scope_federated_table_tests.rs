use super::*;
use subc_protocol::scope::{FederatedAttributes, FederatedPeerClass};

const FED: &str = "callosum";

fn federated(epoch: u64) -> ScopeRecord {
    ScopeRecord::new("f", epoch, ScopeKind::Federated).with_attributes(
        ScopeAttributes::new().with_federated(Some(FederatedAttributes::new(
            FederatedPeerClass::Computer,
            "a".repeat(32),
            "pair:1".into(),
            "Peer".into(),
        ))),
    )
}

fn change(table: &mut ScopeTable, owner: &str, delta: bool, record: ScopeRecord) -> SyncApplied {
    if delta {
        table
            .apply(owner, conn(1), any_current, 2, vec![record], vec![])
            .unwrap()
    } else {
        sync(table, owner, conn(1), 2, vec![record])
    }
}

#[test]
fn federated_records_refuse_local_project_fields_on_sync_and_apply() {
    for delta in [false, true] {
        for record in [
            federated(1).with_workdir(Some("/local/project".into())),
            federated(1).with_project_id(Some("local-project".into())),
        ] {
            let mut table = table();
            sync(&mut table, FED, conn(1), 1, vec![]);
            let result = change(&mut table, FED, delta, record);
            assert_eq!(
                result.results[0].code.as_deref(),
                Some(error_codes::SCOPE_ATTRIBUTE_NOT_PERMITTED)
            );
            assert!(describe(&table, FED, "f").stamp.is_none());
        }
    }
}

#[test]
fn federated_carriers_require_explicit_targets_on_sync_and_apply() {
    for delta in [false, true] {
        let mut table = table();
        sync(&mut table, FED, conn(1), 1, vec![]);
        let record = federated(1).with_carriers(vec![ScopeCarrier::new(reserved(PREFRONTAL))]);
        let result = change(&mut table, FED, delta, record.clone());
        assert_eq!(
            result.results[0].code.as_deref(),
            Some(error_codes::SCOPE_CARRIER_TARGETS_INVALID)
        );
        assert!(describe(&table, FED, "f").stamp.is_none());
        let targeted =
            record
                .with_carriers(vec![ScopeCarrier::new(reserved(PREFRONTAL))
                    .with_targets(Some(vec!["consent".into()]))]);
        assert_eq!(
            sync(&mut table, FED, conn(1), 3, vec![targeted]).results[0].outcome,
            ScopeRecordOutcome::Created
        );
    }
}

#[test]
fn federated_owner_rules_apply_to_sync_and_apply_in_both_directions() {
    for delta in [false, true] {
        let mut variants = vec![
            (PREFRONTAL, federated(1)),
            (FED, head("f", 1)),
            (FED, ScopeRecord::new("f", 1, ScopeKind::Federated)),
            (
                PREFRONTAL,
                head("f", 1).with_attributes(federated(1).attributes),
            ),
        ];
        for attributes in [
            ScopeAttributes::new().with_agent_id(Some("agent".into())),
            ScopeAttributes::new().with_delegates(true),
            ScopeAttributes::new().with_on_behalf_of_agent(Some("agent".into())),
            ScopeAttributes::new().with_flow_id(Some("flow".into())),
            ScopeAttributes::new().with_run_id(Some("run".into())),
            ScopeAttributes::new().with_flow_version(Some(1)),
            ScopeAttributes::new().with_flow_code_hash(Some("a".repeat(64))),
            ScopeAttributes::new().with_flow_install_id(Some("install".into())),
        ] {
            variants.push((
                FED,
                federated(1)
                    .with_attributes(attributes.with_federated(federated(1).attributes.federated)),
            ));
        }
        for (owner, record) in variants {
            let mut table = table();
            sync(&mut table, owner, conn(1), 1, vec![]);
            let result = change(&mut table, owner, delta, record);
            assert_eq!(
                result.results[0].code.as_deref(),
                Some(error_codes::SCOPE_ATTRIBUTE_NOT_PERMITTED)
            );
            assert!(describe(&table, owner, "f").stamp.is_none());
        }
    }
}

#[test]
fn federated_parent_and_child_owner_links_cannot_cross_the_owner_boundary_even_pending() {
    for delta in [false, true] {
        for (owner, record) in [
            (
                FED,
                federated(1).with_child_owners(vec![reserved(PREFRONTAL)]),
            ),
            (
                PREFRONTAL,
                head("f", 1).with_child_owners(vec![reserved(FED)]),
            ),
            (
                FED,
                federated(1).with_parent(Some(ScopeParent::new(reserved(PREFRONTAL), "parent", 1))),
            ),
            (
                PREFRONTAL,
                head("f", 1).with_parent(Some(ScopeParent::new(reserved(FED), "parent", 1))),
            ),
        ] {
            let mut table = table();
            sync(&mut table, owner, conn(1), 1, vec![]);
            let result = change(&mut table, owner, delta, record);
            assert_eq!(
                result.results[0].code.as_deref(),
                Some(error_codes::SCOPE_PARENT_NOT_PERMITTED)
            );
        }
    }
}

#[test]
fn federated_facts_require_a_new_epoch_and_never_authorize_the_owner() {
    for delta in [false, true] {
        // Even a mistaken authority-owner entry cannot confer local authority.
        let mut table = ScopeTable::new([FED.into()]);
        sync(&mut table, FED, conn(1), 1, vec![federated(1)]);
        let original = describe(&table, FED, "f").stamp.unwrap();
        assert!(!original.owner_authorized);
        let mut changed = federated(1);
        changed.attributes.federated.as_mut().unwrap().peer_name = "Renamed".into();
        let result = change(&mut table, FED, delta, changed.clone());
        assert_eq!(
            result.results[0].code.as_deref(),
            Some(error_codes::SCOPE_FEDERATED_CHANGED)
        );
        assert_eq!(describe(&table, FED, "f").stamp.unwrap(), original);
        changed.scope_epoch = 2;
        let result = sync(&mut table, FED, conn(1), 3, vec![changed]);
        assert_eq!(result.results[0].outcome, ScopeRecordOutcome::Replaced);
        let stamp = table
            .admit(
                &reserved(FED),
                "target",
                &ScopeSelector::new(reserved(FED), "f", Some(2)),
                true,
            )
            .unwrap()
            .stamp;
        assert!(!stamp.owner_authorized);
    }
}

#[test]
fn federated_claim_derivation_drain_table_covers_all_identity_inputs() {
    let mut table = table();
    sync(&mut table, PREFRONTAL, conn(1), 1, vec![head("s", 1)]);
    let before = table.owners[PREFRONTAL].live["s"].clone();
    let mut changes = Vec::new();
    for attributes in [
        ScopeAttributes::new().with_agent_id(Some("agent".into())),
        ScopeAttributes::new().with_delegates(true),
        ScopeAttributes::new().with_on_behalf_of_agent(Some("agent".into())),
        ScopeAttributes::new().with_flow_id(Some("flow".into())),
        ScopeAttributes::new().with_run_id(Some("run".into())),
        ScopeAttributes::new().with_flow_version(Some(1)),
        ScopeAttributes::new().with_flow_code_hash(Some("a".repeat(64))),
        ScopeAttributes::new().with_flow_install_id(Some("install".into())),
    ] {
        let mut after = before.clone();
        after.record.attributes = attributes;
        changes.push(after);
    }
    let mut kind = before.clone();
    kind.record.kind = ScopeKind::Worker;
    changes.push(kind);
    let mut authority = before.clone();
    authority.owner_authorized = !before.owner_authorized;
    changes.push(authority);
    for after in changes {
        for (a, b) in [(&before, &after), (&after, &before)] {
            assert_eq!(
                drain_for_change(a, b),
                ScopeDrain::All(RouteCloseReason::ScopeDelegationChanged)
            );
        }
    }
}

#[test]
fn federated_malformed_facts_refuse_the_whole_sync_or_apply_without_advancing_state() {
    for delta in [false, true] {
        let mut table = table();
        sync(&mut table, FED, conn(1), 1, vec![federated(1)]);
        let original = describe(&table, FED, "f").stamp;
        let mut bad = federated(2);
        bad.attributes.federated.as_mut().unwrap().peer_name = "Peer\nforged".into();
        let refusal = if delta {
            table
                .apply(FED, conn(1), any_current, 2, vec![bad], vec![])
                .unwrap_err()
        } else {
            table
                .sync(FED, conn(1), any_current, 2, vec![bad])
                .unwrap_err()
        };
        assert_eq!(refusal.code, INVALID_CONTROL_BODY);
        assert_eq!(describe(&table, FED, "f").stamp, original);
        assert_eq!(
            sync(&mut table, FED, conn(1), 2, vec![federated(1)]).results[0].outcome,
            ScopeRecordOutcome::Unchanged
        );
    }
}
