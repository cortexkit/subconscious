use super::*;
use subc_protocol::PROTOCOL_VERSION;

fn assert_current_facts(registry: &Registry, snapshot: &RouteAdmissionSnapshot) {
    let inner = registry.lock_inner().unwrap();
    assert_eq!(snapshot.modules.len(), inner.modules.len());
    assert_eq!(snapshot.unpublished, inner.unpublished);
    for row in &snapshot.modules {
        assert_eq!(
            row.connection_id,
            inner.modules[&row.module_id].connection_id
        );
    }
    let connections: HashSet<_> = inner
        .modules
        .values()
        .chain(inner.candidates.values())
        .chain(inner.superseded.iter())
        .map(|registration| registration.connection_id)
        .collect();
    assert_eq!(
        snapshot.connections.keys().copied().collect::<HashSet<_>>(),
        connections
    );
    for (connection, row) in &snapshot.connections {
        let (_, original) = inner.find_by_connection(*connection).unwrap();
        assert_eq!(row.module_id, original.manifest.module_id);
        assert_eq!(row.ready, original.ready);
        assert_eq!(row.state, original.state);
        assert_eq!(row.capabilities, original.manifest.capabilities);
        let expected_roles: Vec<_> = original
            .manifest
            .provides
            .iter()
            .filter_map(|role| match role {
                ProviderRole::ToolProvider { .. } => Some("tool".to_owned()),
                ProviderRole::ManagementSurface { .. } => Some("management".to_owned()),
                ProviderRole::InternalService { service_id, .. } => {
                    Some(format!("service:{service_id}"))
                }
                _ => None,
            })
            .collect();
        let actual_roles: Vec<_> = row
            .roles
            .iter()
            .map(|role| match role {
                AdmissionRole::ToolProvider => "tool".to_owned(),
                AdmissionRole::ManagementSurface => "management".to_owned(),
                AdmissionRole::InternalService(service_id) => format!("service:{service_id}"),
            })
            .collect();
        assert_eq!(actual_roles, expected_roles);
    }
}

fn changed(
    registry: &Registry,
    previous: &Arc<RouteAdmissionSnapshot>,
) -> Arc<RouteAdmissionSnapshot> {
    let snapshot = registry.route_admission_snapshot().unwrap();
    assert!(!Arc::ptr_eq(previous, &snapshot));
    assert!(Arc::ptr_eq(
        &snapshot,
        &registry.route_admission_snapshot().unwrap()
    ));
    assert_current_facts(registry, &snapshot);
    snapshot
}

#[test]
fn route_admission_cache_reuses_immutable_views_across_all_registry_lifecycles() {
    let registry = Registry::default();
    let empty = registry.route_admission_snapshot().unwrap();
    let manifest = ModuleManifest::builder("provider", "1.0.0")
        .provides(vec![
            ProviderRole::ToolProvider {
                tools: vec![],
                identity_scope: vec![],
                concurrency: subc_protocol::manifest::Concurrency::StatelessParallel,
                emits_push: false,
                sub_supervises: false,
            },
            ProviderRole::ManagementSurface {
                operations: vec![],
                config_schema: serde_json::json!({"large_schema": "x".repeat(32 * 1024)}),
                observability: vec![],
                identity_scope: vec![],
                concurrency: subc_protocol::manifest::Concurrency::ModuleManaged,
            },
        ])
        .build();
    registry
        .register_with_control_ops(
            manifest.clone(),
            PROTOCOL_VERSION,
            ConnectionId::new(1),
            vec!["route.bind".into()],
        )
        .unwrap();
    let active = changed(&registry, &empty);
    assert!(empty.modules.is_empty());
    assert!(registry
        .grants_control_op("provider", "route.bind")
        .unwrap());
    assert!(!registry.grants_control_op("provider", "missing").unwrap());

    let generation = registry.generation().unwrap();
    registry
        .register_unpublished_with_control_ops(
            ModuleManifest::builder("staged", "1.0.0").build(),
            PROTOCOL_VERSION,
            ConnectionId::new(2),
            vec![],
        )
        .unwrap();
    assert_eq!(registry.generation().unwrap(), generation);
    let staged = changed(&registry, &active);
    assert!(staged.unpublished.contains(&ConnectionId::new(2)));
    registry.publish_registration(ConnectionId::new(2)).unwrap();
    let published = changed(&registry, &staged);

    registry
        .register_unpublished_candidate_with_control_ops(
            manifest,
            PROTOCOL_VERSION,
            ConnectionId::new(3),
            vec![],
        )
        .unwrap();
    let candidate = changed(&registry, &published);
    assert_eq!(candidate.modules[0].connection_id, ConnectionId::new(1));
    registry
        .replace_catalog_for_connection(
            ConnectionId::new(3),
            vec![],
            Some(CapabilityDeclarations {
                provides: vec!["projection/v1".into()],
                requires: vec![],
                must_never_reach: vec!["denied/v1".into()],
            }),
            Some(false),
        )
        .unwrap();
    let updated = changed(&registry, &candidate);
    assert!(candidate.connections[&ConnectionId::new(3)].ready);
    assert!(!updated.connections[&ConnectionId::new(3)].ready);
    registry.publish_registration(ConnectionId::new(3)).unwrap();
    let ready_for_promotion = changed(&registry, &updated);
    registry.promote_candidate("provider").unwrap();
    let promoted = changed(&registry, &ready_for_promotion);
    assert_eq!(promoted.modules[0].connection_id, ConnectionId::new(3));
    assert!(promoted.connections.contains_key(&ConnectionId::new(1)));
    registry
        .deregister_connection(ConnectionId::new(1))
        .unwrap();
    let retired = changed(&registry, &promoted);
    assert!(!retired.connections.contains_key(&ConnectionId::new(1)));
    assert!(promoted.connections.contains_key(&ConnectionId::new(1)));

    registry
        .replace_role_declarations_for_connection(ConnectionId::new(3), BTreeMap::new())
        .unwrap();
    let declarations = changed(&registry, &retired);
    registry
        .set_module_state_for_test("provider", ChannelState::Closed)
        .unwrap();
    let closed = changed(&registry, &declarations);
    registry
        .deregister_connection(ConnectionId::new(3))
        .unwrap();
    let removed = changed(&registry, &closed);
    assert!(removed
        .modules
        .iter()
        .all(|row| row.module_id != "provider"));
    let generation = registry.generation().unwrap();
    registry
        .register_unpublished_with_control_ops(
            ModuleManifest::builder("unpublished", "1.0.0").build(),
            PROTOCOL_VERSION,
            ConnectionId::new(4),
            vec![],
        )
        .unwrap();
    let unpublished = changed(&registry, &removed);
    registry
        .deregister_connection(ConnectionId::new(4))
        .unwrap();
    changed(&registry, &unpublished);
    assert_eq!(registry.generation().unwrap(), generation);
}
