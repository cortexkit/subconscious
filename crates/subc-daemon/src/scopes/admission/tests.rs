use super::*;
use crate::ChannelState;

fn admit_route(
    scopes: &ScopeTable,
    registry: &[ModuleRegistration],
    principal: &Principal,
    target: &str,
    selector: Option<&ScopeSelector>,
    owner_configured: bool,
    mode: AdmissionMode<'_>,
) -> Result<RouteAdmission, Refusal> {
    super::admit_route(
        scopes,
        &RouteAdmissionSnapshot {
            modules: registry.to_vec(),
            unpublished: HashSet::new(),
        },
        principal,
        target,
        selector,
        owner_configured,
        mode,
    )
}

#[test]
fn route_authorize_staged_active_target_is_absent_until_published() {
    let registry = crate::registry::Registry::default();
    let target = registration("target", &[]);
    registry
        .register_unpublished_with_control_ops(
            target.manifest,
            target.negotiated_ver,
            target.connection_id,
            vec![],
        )
        .unwrap();
    assert!(registry.get_module("target").unwrap().is_some());
    assert!(registry.list_modules().unwrap().1.is_empty());
    let snapshot = registry.route_admission_snapshot().unwrap();
    assert_eq!(snapshot.modules.len(), 1);
    let table = table(record());
    assert!(
        super::admit_route(
            &table,
            &snapshot,
            &Principal::Direct,
            "target",
            Some(&selector()),
            true,
            AdmissionMode::Open
        )
        .is_ok(),
        "ordinary opens keep their existing forwarding gate, not a new publication gate"
    );
    let refusal = super::admit_route(
        &table,
        &snapshot,
        &Principal::Direct,
        "target",
        Some(&selector()),
        true,
        AdmissionMode::Authorize(&expected()),
    )
    .err()
    .unwrap();
    assert_eq!(refusal.outcome, Outcome::TargetAbsent);
    assert_eq!(refusal.code, None);
    registry.publish_registration(target.connection_id).unwrap();
    let snapshot = registry.route_admission_snapshot().unwrap();
    assert!(super::admit_route(
        &table,
        &snapshot,
        &Principal::Direct,
        "target",
        Some(&selector()),
        true,
        AdmissionMode::Authorize(&expected())
    )
    .is_ok());
}
use subc_protocol::{
    manifest::ModuleManifest,
    scope::{FederatedAttributes, FederatedPeerClass, RemoteClaim, ScopeCarrier},
    PROTOCOL_VERSION,
};

fn registration(id: &str, provides: &[&str]) -> ModuleRegistration {
    ModuleRegistration {
        manifest: ModuleManifest::builder(id, "1.0.0")
            .capabilities(Some(CapabilityDeclarations {
                provides: provides.iter().map(|s| (*s).into()).collect(),
                requires: vec![],
                must_never_reach: vec![],
            }))
            .build(),
        ready: true,
        negotiated_ver: PROTOCOL_VERSION,
        state: ChannelState::Active,
        connection_id: ConnectionId::new(if id == "target" { 1 } else { 2 }),
        control_ops: vec![],
    }
}

fn record() -> ScopeRecord {
    ScopeRecord::new("source", 1, ScopeKind::Head)
        .with_attributes(ScopeAttributes::new().with_agent_id(Some("agent".into())))
        .with_carriers(vec![
            ScopeCarrier::new(Principal::Direct),
            ScopeCarrier::new(reserved("carrier")),
        ])
}

fn table(record: ScopeRecord) -> ScopeTable {
    let mut table = ScopeTable::new(["owner".into()]);
    table.set_wall_clock(Arc::new(|| 100));
    sync(&mut table, 1, record);
    table
}

fn sync(table: &mut ScopeTable, generation: u64, record: ScopeRecord) {
    let applied = table
        .sync(
            "owner",
            ConnectionId::new(10),
            |_| true,
            generation,
            vec![record],
        )
        .unwrap();
    assert!(
        applied.results.iter().all(|r| r.code.is_none()),
        "{applied:?}"
    );
}

fn selector() -> ScopeSelector {
    ScopeSelector::new(reserved("owner"), "source", Some(1))
}
fn expected() -> RouteAuthorizeExpected {
    RouteAuthorizeExpected::new(ScopeKind::Head, &record().attributes)
}

#[test]
/// For matching claims, authorized owners and settled links, authorization
/// succeeds exactly when Open's shared checks admit against the same snapshot.
/// Open's role and transient availability checks are deliberately outside this
/// domain; stricter authorize-only facts have their own tests below.
fn route_authorize_open_parity_every_admission_refusal() {
    // Each case isolates one predicate. Authorization-only claim/link facts
    // are exercised separately because an ordinary bind may carry those facts.
    for case in [
        "authorized",
        "owner_type",
        "unconfigured",
        "unsynced",
        "epoch",
        "absent",
        "deadline",
        "proof",
        "carrier",
        "deny",
        "flow",
        "run",
        "worker_run",
        "delegating_run",
        "federated",
        "stamp_required",
        "target_absent",
        "target_not_ready",
    ] {
        let mut record = record();
        let mut selector = selector();
        let mut registry = vec![registration("target", &[]), registration("carrier", &[])];
        let mut principal = Principal::Direct;
        let mut configured = true;
        match case {
            "owner_type" => selector.owner = Principal::Direct,
            "unconfigured" => {
                selector.owner = reserved("unknown");
                configured = false;
            }
            "unsynced" => selector.owner = reserved("unknown"),
            "epoch" => selector.scope_epoch = Some(2),
            "absent" => selector.scope_ref = "absent".into(),
            "deadline" => record.expires_at_ms = Some(101),
            "proof" => record.bind_proof_sha256 = Some("a".repeat(64)),
            "carrier" => record.carriers.clear(),
            "deny" => {
                principal = reserved("carrier");
                registry[0].manifest.capabilities.as_mut().unwrap().provides =
                    vec!["forbidden/v1".into()];
                registry[1]
                    .manifest
                    .capabilities
                    .as_mut()
                    .unwrap()
                    .must_never_reach = vec!["forbidden/v1".into()];
            }
            "flow" => record.attributes.flow_id = Some("flow".into()),
            "run" => record.attributes.run_id = Some("run".into()),
            "worker_run" => {
                record.attributes.agent_id = None;
                record.attributes.on_behalf_of_agent = Some("agent".into());
                record.attributes.run_id = Some("run".into());
            }
            "delegating_run" => {
                record.attributes.run_id = Some("run".into());
                record.attributes.delegates = true;
                registry[0].manifest.capabilities.as_mut().unwrap().provides =
                    vec![AGENT_RUN_SCOPES_CAPABILITY.into()];
            }
            "target_absent" => {
                registry.remove(0);
            }
            "target_not_ready" => registry[0].ready = false,
            _ => {}
        }
        let mut table = table(record.clone());
        if case == "deadline" {
            table.set_wall_clock(Arc::new(|| 102));
        }
        if matches!(case, "federated" | "stamp_required") {
            // A federated scope cannot confer local authority. Isolate target
            // support here; the owner-authorized rule is covered below.
            let scope = table
                .owners
                .get_mut("owner")
                .unwrap()
                .live
                .get_mut("source")
                .unwrap();
            scope.record.kind = ScopeKind::Federated;
            scope.record.attributes = ScopeAttributes::new().with_federated(Some(
                FederatedAttributes::new(
                    FederatedPeerClass::Computer,
                    "a".repeat(32),
                    "pair".into(),
                    "Peer".into(),
                )
                .with_remote_claim(Some(RemoteClaim::run("agent"))),
            ));
            scope
                .record
                .attributes
                .federated
                .as_mut()
                .unwrap()
                .stamp_required = case == "stamp_required";
            record = scope.record.clone();
        }
        let expected = RouteAuthorizeExpected::new(record.kind, &record.attributes);
        let open = admit_route(
            &table,
            &registry,
            &principal,
            "target",
            Some(&selector),
            configured,
            AdmissionMode::Open,
        );
        let authorize = admit_route(
            &table,
            &registry,
            &principal,
            "target",
            Some(&selector),
            configured,
            AdmissionMode::Authorize(&expected),
        );
        assert_eq!(open.is_ok(), authorize.is_ok(), "parity: {case}");
        let (outcome, code) = match case {
            "authorized" => {
                assert!(authorize
                    .unwrap()
                    .content_version
                    .unwrap()
                    .starts_with("sha256:"));
                continue;
            }
            "owner_type" | "unconfigured" | "absent" | "deadline" => {
                (Outcome::ScopeEnded, Some(error_codes::SCOPE_NOT_LIVE))
            }
            "unsynced" => (Outcome::Indeterminate, Some(error_codes::SCOPE_NOT_SYNCED)),
            "epoch" => (Outcome::EpochChanged, Some(error_codes::SCOPE_ENDED)),
            "proof" => (
                Outcome::ProofRequired,
                Some(error_codes::SCOPE_BIND_PROOF_REQUIRED),
            ),
            "carrier" => (Outcome::NotAuthorized, Some(error_codes::SCOPE_NOT_CARRIER)),
            "deny" => (Outcome::NotAuthorized, Some("capability_forbidden")),
            "flow" => (
                Outcome::NotAuthorized,
                Some(error_codes::TARGET_FLOW_UNSUPPORTED),
            ),
            "run" | "worker_run" | "delegating_run" => (
                Outcome::NotAuthorized,
                Some(error_codes::TARGET_AGENT_RUN_UNSUPPORTED),
            ),
            "federated" => (
                Outcome::NotAuthorized,
                Some(error_codes::TARGET_FEDERATED_UNSUPPORTED),
            ),
            "stamp_required" => (
                Outcome::NotAuthorized,
                Some(error_codes::FEDERATED_STAMP_REQUIRED),
            ),
            "target_absent" | "target_not_ready" => (Outcome::TargetAbsent, None),
            _ => unreachable!(),
        };
        let authorize = authorize.err().unwrap();
        assert_eq!(authorize.outcome, outcome, "{case}");
        assert_eq!(authorize.code, code, "{case}");
        assert_eq!(open.err().unwrap().code, code, "{case}");
    }
}

#[test]
fn route_authorize_ordering_links_claim_and_principal_registration() {
    let mut table = table(record());
    let mut registry = vec![registration("target", &[])];
    let mut selector = selector();
    let check = |table: &ScopeTable,
                 registry: &[ModuleRegistration],
                 selector: &ScopeSelector,
                 principal: &Principal,
                 expected: &RouteAuthorizeExpected| {
        admit_route(
            table,
            registry,
            principal,
            "target",
            Some(selector),
            true,
            AdmissionMode::Authorize(expected),
        )
        .err()
        .unwrap()
    };
    selector.owner = reserved("unsynced");
    selector.scope_ref = "missing".into();
    assert_eq!(
        check(&table, &[], &selector, &Principal::Unverified, &expected()).outcome,
        Outcome::Indeterminate
    );
    selector = self::selector();
    for (state, answer) in [
        (ParentState::Pending, Outcome::Indeterminate),
        (ParentState::Ended, Outcome::ScopeEnded),
    ] {
        table
            .owners
            .get_mut("owner")
            .unwrap()
            .live
            .get_mut("source")
            .unwrap()
            .parent_state = Some(state);
        let refusal = check(&table, &[], &selector, &Principal::Unverified, &expected());
        assert_eq!(refusal.outcome, answer);
        assert_eq!(refusal.code, None);
    }
    let scope = table
        .owners
        .get_mut("owner")
        .unwrap()
        .live
        .get_mut("source")
        .unwrap();
    scope.parent_state = None;
    scope.owner_authorized = false;
    assert_eq!(
        check(
            &table,
            &registry,
            &selector,
            &Principal::Direct,
            &expected()
        )
        .outcome,
        Outcome::ClaimChanged
    );
    table
        .owners
        .get_mut("owner")
        .unwrap()
        .live
        .get_mut("source")
        .unwrap()
        .owner_authorized = true;
    for field in [
        "kind",
        "agent_id",
        "delegates",
        "run_id",
        "on_behalf_of_agent",
        "flow_id",
        "flow_version",
        "flow_install_id",
        "flow_code_hash",
    ] {
        let mut value = serde_json::to_value(expected()).unwrap();
        value[field] = match field {
            "kind" => serde_json::json!("worker"),
            "delegates" => serde_json::json!(true),
            "flow_version" => serde_json::json!(2),
            "agent_id" => serde_json::Value::Null,
            _ => serde_json::json!("different"),
        };
        let expected = serde_json::from_value(value).unwrap();
        assert_eq!(
            check(&table, &registry, &selector, &Principal::Direct, &expected).outcome,
            Outcome::ClaimChanged,
            "{field}"
        );
    }
    assert_eq!(
        check(
            &table,
            &registry,
            &selector,
            &reserved("carrier"),
            &expected()
        )
        .outcome,
        Outcome::Indeterminate
    );
    registry.push(registration("carrier", &[]));
    assert!(admit_route(
        &table,
        &registry,
        &reserved("carrier"),
        "target",
        Some(&selector),
        true,
        AdmissionMode::Authorize(&expected())
    )
    .is_ok());
}

#[test]
fn route_authorize_removed_configured_owner_ends_but_open_keeps_compatibility() {
    let table = table(record());
    let registry = [registration("target", &[])];
    assert!(admit_route(
        &table,
        &registry,
        &Principal::Direct,
        "target",
        Some(&selector()),
        false,
        AdmissionMode::Open
    )
    .is_ok());
    let refusal = admit_route(
        &table,
        &registry,
        &Principal::Direct,
        "target",
        Some(&selector()),
        false,
        AdmissionMode::Authorize(&expected()),
    )
    .err()
    .unwrap();
    assert_eq!(refusal.outcome, Outcome::ScopeEnded);
    assert_eq!(refusal.code, Some(error_codes::SCOPE_NOT_LIVE));
}

#[test]
fn route_authorize_multiple_refusals_preserve_each_modes_order() {
    let mut table = table(record());
    table.owners.get_mut("owner").unwrap().synced = false;
    let open = admit_route(
        &table,
        &[],
        &Principal::Unverified,
        "target",
        Some(&selector()),
        true,
        AdmissionMode::Open,
    )
    .err()
    .unwrap();
    let authorize = admit_route(
        &table,
        &[],
        &Principal::Unverified,
        "target",
        Some(&selector()),
        true,
        AdmissionMode::Authorize(&expected()),
    )
    .err()
    .unwrap();
    assert_eq!(open.outcome, Outcome::TargetAbsent);
    assert_eq!(authorize.outcome, Outcome::Indeterminate);
    assert_eq!(authorize.code, Some(error_codes::SCOPE_NOT_SYNCED));
}

#[test]
fn route_authorize_content_version_survives_restart_and_normalizes_carriers() {
    let version = |table: &ScopeTable| {
        admit_route(
            table,
            &[registration("target", &[])],
            &Principal::Direct,
            "target",
            Some(&selector()),
            true,
            AdmissionMode::Authorize(&expected()),
        )
        .unwrap()
        .content_version
        .unwrap()
    };
    let original = record();
    let mut first = table(original.clone());
    let initial = version(&first);
    assert_eq!(initial.len(), 71);
    let mut restarted = table(original.clone());
    restarted.last_version = 98;
    sync(&mut restarted, 2, original.clone());
    assert_eq!(initial, version(&restarted));
    let mut edited = original.clone();
    edited.carriers.push(ScopeCarrier::new(reserved("other")));
    sync(&mut first, 2, edited.clone());
    assert_eq!(initial, version(&first));
    edited.carriers[0].targets = Some(vec!["target".into(), "second".into()]);
    sync(&mut first, 3, edited.clone());
    let narrowed = version(&first);
    assert_ne!(initial, narrowed);
    edited.carriers[0].targets = Some(vec!["second".into(), "target".into(), "target".into()]);
    sync(&mut first, 4, edited.clone());
    assert_eq!(narrowed, version(&first));
    edited.carriers[0].targets = Some(vec!["target".into()]);
    sync(&mut first, 5, edited);
    assert_ne!(narrowed, version(&first));
    sync(&mut first, 6, original);
    assert_eq!(initial, version(&first));
}

#[test]
fn route_authorize_content_version_covers_identity_link_proof_and_expiry_not_display_or_registration(
) {
    let mut table = table(record());
    let selected = selector();
    let scope = table
        .owners
        .get_mut("owner")
        .unwrap()
        .live
        .get_mut("source")
        .unwrap();
    scope.record.attributes.federated = Some(
        FederatedAttributes::new(
            FederatedPeerClass::Computer,
            "a".repeat(32),
            "pair".into(),
            "Display".into(),
        )
        .with_remote_claim(Some(RemoteClaim::agent("remote"))),
    );
    let initial = content_version(scope, &selected, &Principal::Direct);
    let mut renamed_peer = scope.clone();
    renamed_peer
        .record
        .attributes
        .federated
        .as_mut()
        .unwrap()
        .peer_name = "Renamed peer".into();
    assert_ne!(
        initial,
        content_version(&renamed_peer, &selected, &Principal::Direct),
        "peer_name is an epoch-fixed pairing-roster fact"
    );
    for edit in [
        "epoch",
        "kind",
        "attributes",
        "authorized",
        "parent",
        "parent_state",
        "proof",
        "expiry",
    ] {
        let mut changed = scope.clone();
        match edit {
            "epoch" => changed.record.scope_epoch += 1,
            "kind" => changed.record.kind = ScopeKind::Worker,
            "attributes" => changed.record.attributes.agent_id = Some("other-agent".into()),
            "authorized" => changed.owner_authorized = false,
            "parent" => {
                changed.record.parent =
                    Some(ScopeParent::new(reserved("parent-owner"), "parent", 1))
            }
            "parent_state" => changed.parent_state = Some(ParentState::Linked),
            "proof" => changed.record.bind_proof_sha256 = Some("a".repeat(64)),
            "expiry" => changed.record.expires_at_ms = Some(200),
            _ => unreachable!(),
        }
        assert_ne!(
            initial,
            content_version(&changed, &selected, &Principal::Direct),
            "{edit}"
        );
    }
    let mut renamed_owner = selected.clone();
    renamed_owner.owner = reserved("other-owner");
    assert_ne!(
        initial,
        content_version(scope, &renamed_owner, &Principal::Direct)
    );
    let mut renamed_ref = selected;
    renamed_ref.scope_ref = "other-ref".into();
    assert_ne!(
        initial,
        content_version(scope, &renamed_ref, &Principal::Direct)
    );
    let table = self::table(record());
    let first = admit_route(
        &table,
        &[registration("target", &[])],
        &Principal::Direct,
        "target",
        Some(&selector()),
        true,
        AdmissionMode::Authorize(&expected()),
    )
    .unwrap()
    .content_version;
    let mut replaced = registration("target", &["extra-capability/v1"]);
    replaced.connection_id = ConnectionId::new(987);
    let second = admit_route(
        &table,
        &[replaced],
        &Principal::Direct,
        "target",
        Some(&selector()),
        true,
        AdmissionMode::Authorize(&expected()),
    )
    .unwrap()
    .content_version;
    assert_eq!(
        first, second,
        "registrations are judged live, not part of scope content"
    );
}

#[test]
fn route_authorize_content_version_ignores_remote_display_names_but_not_claim_identity() {
    let table = table(record());
    let selected = selector();
    let base = &table.owners["owner"].live["source"];
    for claim in [
        RemoteClaim::agent("remote"),
        RemoteClaim::run("remote"),
        RemoteClaim::worker("remote-owner", "a".repeat(64), Some("remote".into())),
        RemoteClaim::flow("flow", 1, "install", "a".repeat(64)),
    ] {
        let mut original = base.clone();
        original.record.attributes.federated = Some(
            FederatedAttributes::new(
                FederatedPeerClass::Computer,
                "a".repeat(32),
                "pair".into(),
                "Peer".into(),
            )
            .with_remote_claim(Some(claim.clone())),
        );
        let version = content_version(&original, &selected, &Principal::Direct);
        let renamed = claim
            .clone()
            .with_agent_name("Agent display")
            .with_flow_name("Flow display");
        assert!(claim.same_identity(&renamed));
        let mut changed = original.clone();
        changed
            .record
            .attributes
            .federated
            .as_mut()
            .unwrap()
            .remote_claim = Some(renamed);
        assert_eq!(
            version,
            content_version(&changed, &selected, &Principal::Direct)
        );
        let mut wire = serde_json::to_value(&claim).unwrap();
        let identity = if wire.get("agent_id").is_some() {
            "agent_id"
        } else if wire.get("worker_ref").is_some() {
            "worker_ref"
        } else {
            "flow_id"
        };
        wire[identity] = serde_json::json!(if identity == "worker_ref" {
            "b".repeat(64)
        } else {
            "changed".into()
        });
        let new_claim: RemoteClaim = serde_json::from_value(wire).unwrap();
        assert!(!claim.same_identity(&new_claim));
        changed
            .record
            .attributes
            .federated
            .as_mut()
            .unwrap()
            .remote_claim = Some(new_claim);
        assert_ne!(
            version,
            content_version(&changed, &selected, &Principal::Direct)
        );
    }
}
