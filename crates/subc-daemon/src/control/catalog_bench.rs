use super::*;

fn realistic_manifest(module_id: &str) -> ModuleManifest {
    let tools = (0..24).map(|index| Tool {
        name: format!("records_{index}"),
        description: Some("Read matching records with their metadata, ordering and continuation cursor. No records are modified by this operation.".into()),
        execution_mode: ExecutionMode::Pure,
        schema: json!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Canonical project directory to search."},
                "query": {"type": "string", "description": "Search expression matching record titles and content."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000},
                "cursor": {"type": "string", "description": "Continuation cursor from a previous response."},
                "include_metadata": {"type": "boolean", "default": true}
            },
            "required": ["project", "query"],
            "additionalProperties": false
        }),
    }).collect();
    ModuleManifest::builder(module_id, "1.2.3")
        .provides(vec![
            ProviderRole::ToolProvider {
                tools,
                identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
                concurrency: Concurrency::ModuleManaged,
                emits_push: true,
                sub_supervises: false,
            },
            ProviderRole::ManagementSurface {
                operations: (0..16).map(|index| ManagementOperation {
                    name: format!("records.query_{index}"),
                    kind: ManagementOperationKind::Query,
                    description: Some("Return the selected records and their current metadata without changing their contents.".into()),
                }).collect(),
                config_schema: json!({"type": "object", "properties": {"enabled": {"type": "boolean"}}}),
                observability: vec![],
                identity_scope: vec![IdentityScope::Project],
                concurrency: Concurrency::ModuleManaged,
            },
        ])
        .build()
}

fn cpu_time() -> Duration {
    let usage = subc_os::resource_usage(std::process::id()).unwrap();
    usage.cpu_user + usage.cpu_system
}

#[test]
#[ignore = "explicit synthetic CPU measurement; includes two 10/s load windows"]
fn catalog_cache_synthetic_load_cpu() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(registry.clone());
    for index in 0..25 {
        registry
            .register_with_control_ops(
                realistic_manifest(&format!("catalog-bench-{index:02}")),
                PROTOCOL_VERSION,
                ConnectionId::new(index + 1),
                vec!["health.check".into(), "route.bind".into()],
            )
            .unwrap();
    }
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        99,
        br#"{"op":"catalog.list"}"#.to_vec(),
    )
    .unwrap();
    let cached = handler.handle_catalog_list(request.clone(), None).unwrap();
    let uncached = handler.uncached_catalog_list(99, None).unwrap();
    assert_eq!(cached[0].body, serde_json::to_vec(&uncached).unwrap());
    eprintln!("catalog_cpu fixture modules=25 tools_per_module=24 management_ops_per_module=16 body_bytes={}", cached[0].body.len());
    for use_cache in [false, true] {
        let call = || {
            let response = if use_cache {
                handler
                    .handle_catalog_list(request.clone(), None)
                    .unwrap()
                    .remove(0)
            } else {
                let response = handler.uncached_catalog_list(99, None).unwrap();
                control_response_body_frame(&request, &response, "catalog CPU fixture").unwrap()
            };
            std::hint::black_box(response);
        };
        let mode = if use_cache { "cached" } else { "uncached" };
        let started = Instant::now();
        let cpu = cpu_time();
        for index in 0..100 {
            call();
            std::thread::sleep(
                (started + Duration::from_millis((index + 1) * 100))
                    .saturating_duration_since(Instant::now()),
            );
        }
        let used = cpu_time().saturating_sub(cpu);
        eprintln!("catalog_cpu mode={mode} load=10/s calls=100 cpu_ms={} cpu_us_per_call={:.3} wall_ms={}", used.as_millis(), used.as_secs_f64() * 1e6 / 100.0, started.elapsed().as_millis());
        let started = Instant::now();
        let cpu = cpu_time();
        for _ in 0..20_000 {
            call();
        }
        let used = cpu_time().saturating_sub(cpu);
        eprintln!("catalog_cpu mode={mode} load=burst calls=20000 cpu_ms={} cpu_us_per_call={:.3} wall_ms={}", used.as_millis(), used.as_secs_f64() * 1e6 / 20_000.0, started.elapsed().as_millis());
    }
}
