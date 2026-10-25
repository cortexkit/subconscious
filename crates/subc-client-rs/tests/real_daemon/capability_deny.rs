use super::*;

struct HeldCallHandler {
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ModuleHandler for HeldCallHandler {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        // The request has reached the provider. Leave its response pending so
        // route teardown, rather than a terminal response, settles the call.
        self.started.notify_one();
        std::future::pending().await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_deny_settles_accepted_call_on_real_daemon() {
    const OPENER: &str = "capability-deny-opener";
    const TARGET: &str = "capability-deny-target";
    const CAPABILITY: &str = "credentials-provider/v1";

    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let stub_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "fake-aft-stub",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-capability-deny");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    let nonce_path = temp_dir.join("opener.nonce");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    let config = json!({
        "version": 1,
        "modules": {
            OPENER: {
                "program": stub_bin.to_string_lossy(),
                "args": [],
                "env": {
                    "FAKE_AFT_MODULE_ID": OPENER,
                    "FAKE_AFT_LAUNCH_NONCE_PATH": nonce_path.to_string_lossy(),
                    "FAKE_AFT_CAPABILITIES": json!({
                        "provides": [],
                        "requires": [],
                        "must_never_reach": [CAPABILITY],
                    }).to_string(),
                },
                "enabled": true,
                "reserved": true,
            },
        },
    });
    fs::write(
        config_dir.join("cortexkit/subc.jsonc"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, OPENER, START_TIMEOUT).await;

    let started = Arc::new(tokio::sync::Notify::new());
    let manifest = inline_module_manifest(TARGET, &["hold"]);
    let roles = manifest.provides.clone();
    let (module, serving) = spawn_inline_module_with_handler(
        &daemon.connection_file,
        manifest,
        HeldCallHandler {
            started: Arc::clone(&started),
        },
    )
    .await;
    let consumer = Arc::new(
        SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
            .await
            .unwrap(),
    );
    let options = CallOptions {
        timeout: Duration::from_secs(60),
        consumer_identity: Some(ConsumerIdentity {
            module_id: OPENER.to_string(),
            launch_nonce: wait_for_file(&nonce_path).await,
        }),
        ..fast_call_options()
    };
    let route = consumer
        .open_route(
            tool_target(TARGET),
            consumer_identity("deny-call"),
            options.clone(),
        )
        .await
        .unwrap();
    let pending = {
        let consumer = Arc::clone(&consumer);
        tokio::spawn(async move { consumer.request(&route, b"hold".to_vec(), options).await })
    };
    timeout(EVENT_TIMEOUT, started.notified())
        .await
        .expect("the provider must accept the call before its route is denied");

    // Declaring the capability as provided makes the opener's must_never_reach
    // restriction apply to this existing route, without disconnecting either peer.
    module
        .catalog_update_with_capabilities(
            roles,
            CapabilityDeclarations {
                provides: vec![CAPABILITY.to_string()],
                requires: Vec::new(),
                must_never_reach: Vec::new(),
            },
        )
        .await
        .unwrap();
    let err = timeout(EVENT_TIMEOUT, pending)
        .await
        .expect("capability denial must settle the accepted call before its 60s deadline")
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, CallError::OutcomeUnknown(_)), "{err}");
    assert_eq!(err.outcome_cause(), Some(OutcomeUnknownCause::RouteEnded));
    assert_eq!(
        err.close_reason(),
        Some(&RouteEndReason::Daemon(RouteCloseReason::CapabilityDenied))
    );
    assert!(!serving.is_finished(), "the provider connection stays up");
    let stale = consumer
        .request(&route, b"again".to_vec(), fast_call_options())
        .await
        .unwrap_err();
    assert!(matches!(stale, CallError::StaleRouteHandle(_)), "{stale}");

    serving.abort();
    daemon.kill_and_wait();
}
