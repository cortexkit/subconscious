mod harness;
// Only the refusing defaults are exercised here; the seam types the grant and bootstrap
// areas use are compiled but not constructed.
#[allow(dead_code)]
#[path = "../src/runtime/seams.rs"]
mod runtime_seams;

use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, Instant},
};

use harness::{control, daemon::AcceptanceRun, data_home, stubs};
use subc_control::{ClientControlRequest, ClientControlResponse, ConsumerIdentity, ModuleProtocol};
use subc_protocol::{BindIdentity, Principal, RouteTarget};
use syn::{visit::Visit, Expr, ImplItemConst, ItemConst, Lit, TraitItemConst, Type};
use tokio::process::Command;

const MODULE_ID: &str = "ckbus";
const DECLARED_DRAIN_TIMEOUT_MS: u64 = 2_000;

fn state_constants(source: &str) -> Vec<(String, String)> {
    #[derive(Default)]
    struct StateConstants(Vec<(String, String)>);

    impl StateConstants {
        fn record(&mut self, ident: &syn::Ident, ty: &Type, expr: &Expr) {
            let name = ident.to_string();
            let is_str = matches!(ty, Type::Reference(reference)
                if reference.mutability.is_none()
                && matches!(reference.elem.as_ref(), Type::Path(ty) if ty.path.is_ident("str")));
            if is_str && (name.ends_with("_FILE") || name.ends_with("_DIR")) {
                if let Expr::Lit(literal) = expr {
                    if let Lit::Str(value) = &literal.lit {
                        self.0.push((name, value.value()));
                    }
                }
            }
        }
    }

    impl<'ast> Visit<'ast> for StateConstants {
        fn visit_item_const(&mut self, node: &'ast ItemConst) {
            self.record(&node.ident, &node.ty, &node.expr);
            syn::visit::visit_item_const(self, node);
        }

        fn visit_impl_item_const(&mut self, node: &'ast ImplItemConst) {
            self.record(&node.ident, &node.ty, &node.expr);
            syn::visit::visit_impl_item_const(self, node);
        }

        fn visit_trait_item_const(&mut self, node: &'ast TraitItemConst) {
            if let Some((_, expr)) = &node.default {
                self.record(&node.ident, &node.ty, expr);
            }
            syn::visit::visit_trait_item_const(self, node);
        }
    }

    // Parse declarations rather than lines so visibility and rustfmt's line
    // wrapping cannot hide a state-file constant from the classification guard.
    let parsed = syn::parse_file(source).expect("ck-bus source must parse");
    let mut constants = StateConstants::default();
    constants.visit_file(&parsed);
    constants.0
}

#[test]
fn state_constants_positive_control_recognizes_declaration_spellings() {
    let source = r#"
        pub const PUBLIC_FILE: &str = "public.json";
        pub(crate) const CRATE_DIR: &str = "crate";
        const PRIVATE_FILE: &str = "private.json";
        pub const MULTILINE_FILE: &str =
            "multiline.json";
        mod nested {
            pub(super) const SUPER_DIR: &'static str = r"nested";
        }
        impl State {
            pub const ASSOCIATED_FILE: &str = "associated.json";
        }
        trait Defaults {
            const DEFAULT_DIR: &str = "defaults";
        }
        pub const X_PATH: &str = "not-a-state-file";
        pub const NUMBER_FILE: u64 = 1;
        const BYTES_DIR: &[u8] = b"not-a-string";
    "#;
    assert_eq!(
        state_constants(source),
        vec![
            ("PUBLIC_FILE".into(), "public.json".into()),
            ("CRATE_DIR".into(), "crate".into()),
            ("PRIVATE_FILE".into(), "private.json".into()),
            ("MULTILINE_FILE".into(), "multiline.json".into()),
            ("SUPER_DIR".into(), "nested".into()),
            ("ASSOCIATED_FILE".into(), "associated.json".into()),
            ("DEFAULT_DIR".into(), "defaults".into()),
        ],
        "the state-file scan must recognize every planted spelling and exclude non-state constants"
    );
}

/// The acceptance tests fingerprint the operator's real ck-bus data directory
/// (`$XDG_DATA_HOME/cortexkit/ckbus`) before and after a run and compare the two,
/// to prove the tests never wrote to the operator's install. The operator's own
/// ck-bus may be running meanwhile, so `data_home` lists which files it rewrites
/// while serving (compared by presence only) and which it never touches
/// (compared by content). A file in neither list is compared by content, so a
/// new state file the live module rewrites during a run would fail an unrelated
/// test. This reads every `const *_FILE` / `*_DIR` string in ck-bus's own source
/// and requires each to be in one of the lists.
#[test]
fn every_ckbus_state_file_is_classified() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = BTreeSet::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            for (name, value) in state_constants(&std::fs::read_to_string(&path).unwrap()) {
                found.insert((value, format!("{}:{name}", path.display())));
            }
        }
    }
    assert!(
        !found.is_empty(),
        "the scan found no state-file constants, so it is reading the wrong place"
    );
    eprintln!("ck-bus state-file inventory: {} constants", found.len());
    let classified: BTreeSet<&str> = data_home::LIVE_REWRITTEN
        .iter()
        .chain(data_home::LIVE_SUBTREES.iter())
        .chain(data_home::CONTENT_COMPARED.iter())
        .copied()
        .collect();
    let unclassified: Vec<_> = found
        .iter()
        .filter(|(value, _)| !classified.contains(value.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "ck-bus state files missing from the operator data-home guard's lists in \
         tests/harness/data_home.rs: {unclassified:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn module_declaration_and_registration_names_every_observable() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let operator_dir = data_home::operator_module_dir();
    let operator_before = data_home::fingerprint(operator_dir.as_deref());
    let binary = Path::new(env!("CARGO_BIN_EXE_ck-bus"));
    let run = AcceptanceRun::start(binary).await;
    // The operator fingerprint above is evidence ONLY while the directory it names lies
    // outside this run's fixture tree. `operator_module_dir` resolves through
    // XDG_DATA_HOME, which this test overrides on the CHILD command rather than on
    // itself -- correct today, and silently vacuous the day any helper sets that
    // variable on the test process, because both fingerprints would then describe the
    // fixture and agree for the wrong reason. Assert the separation rather than trusting
    // the ordering to hold.
    if let Some(dir) = operator_dir.as_deref() {
        assert!(
            !dir.starts_with(run.root.path()),
            "observable operator data home {} must lie outside the fixture tree {}, else \
             the unchanged-fingerprint assertion proves nothing",
            dir.display(),
            run.root.path().display()
        );
    }

    let list_entry = wait_for_live_supervisor_entry(&run).await;
    assert_eq!(
        list_entry.module_id, MODULE_ID,
        "observable supervisor.list module_id must be ckbus"
    );
    assert_eq!(
        list_entry.protocol,
        ModuleProtocol::Subc,
        "observable supervisor.list protocol must be subc"
    );
    assert_eq!(
        list_entry.drain_timeout_ms,
        Some(DECLARED_DRAIN_TIMEOUT_MS),
        "observable supervisor.list drain_timeout_ms must equal the declaration"
    );
    assert!(
        list_entry.live,
        "observable supervisor.list live must be true after registration"
    );

    let supervised_pid = wait_for_narrowed_provenance_pid(&run).await;
    let catalog = catalog_entries(&run, MODULE_ID).await;
    assert_eq!(
        catalog.len(),
        1,
        "observable catalog.list registration count for ckbus must be exactly one"
    );
    assert!(
        catalog[0].control_ops.iter().any(|op| op == "health.check"),
        "observable CatalogEntry.control_ops must advertise health.check"
    );

    assert_stub_catalog(&run, "claustrum", stubs::CLAUSTRUM_OPERATIONS).await;
    assert_stub_catalog(&run, "callosum", stubs::CALLOSUM_OPERATIONS).await;
    assert_eq!(
        run.claustrum.operations(),
        stubs::CLAUSTRUM_OPERATIONS
            .iter()
            .map(|op| (*op).to_string())
            .collect(),
        "observable claustrum harness stub operation registry must match its catalog"
    );
    assert_eq!(
        run.callosum.operations(),
        stubs::CALLOSUM_OPERATIONS
            .iter()
            .map(|op| (*op).to_string())
            .collect(),
        "observable callosum harness stub operation registry must match its catalog"
    );
    assert!(
        run.claustrum.observed_principals().is_empty(),
        "observable claustrum principal log must start empty before any route binds"
    );
    assert!(
        run.callosum.observed_principals().is_empty(),
        "observable callosum principal log must start empty before any route binds"
    );
    assert_stub_principal_controls(&run).await;
    let _claustrum_reply_registration_seam = run.claustrum.replies();
    let _callosum_reply_registration_seam = run.callosum.replies();

    assert_hand_started_copy_refused(&run, binary, None).await;
    assert_hand_started_copy_refused(&run, binary, Some("altered-launch-nonce")).await;

    let pid_after_refusals = wait_for_narrowed_provenance_pid(&run).await;
    assert_eq!(pid_after_refusals, supervised_pid, "observable supervisor.provenance daemon_observed.pid must stay P after refused hand starts");
    let catalog_after_refusals = catalog_entries(&run, MODULE_ID).await;
    assert_eq!(catalog_after_refusals.len(), 1, "observable catalog.list must still contain exactly the supervised registration after refused hand starts");

    assert_reserved_none_declaration_is_refused(&run.config_file);
    record_server_describe(&run).await;
    run.shutdown().await;

    let operator_after = data_home::fingerprint(operator_dir.as_deref());
    assert_eq!(
        operator_after, operator_before,
        "observable operator real data-home ckbus tree must be byte-for-byte unchanged"
    );
}

#[test]
fn fixture_declares_all_slice_owned_module_blocks_and_no_shipped_sentinel_override() {
    let value = harness::config::template_value();
    let modules = value["modules"]
        .as_object()
        .expect("observable fixture modules must be an object");
    for module_id in [
        "ckbus",
        "nats-server",
        "standin-none",
        "standin-default-protocol",
        "standin-ignores-term",
        "claustrum",
        "callosum",
    ] {
        assert!(
            modules.contains_key(module_id),
            "observable fixture module block {module_id} must exist"
        );
    }
    assert_eq!(
        modules["nats-server"]["protocol"], "none",
        "observable nats-server protocol must be none"
    );
    assert_eq!(
        modules["standin-none"]["protocol"], "none",
        "observable explicit stand-in protocol must be none"
    );
    assert_eq!(
        modules["standin-ignores-term"]["protocol"], "none",
        "observable SIGTERM-ignoring control stand-in protocol must be none"
    );
    assert_eq!(
        modules["standin-ignores-term"]["program"],
        "crates/ck-bus/tests/support/standin_ignores_term.sh",
        "observable control stand-in program path must name its support script"
    );
    assert!(
        modules["standin-default-protocol"]
            .get("protocol")
            .is_none(),
        "observable omitted-protocol stand-in must omit the protocol key"
    );
    for module_id in ["standin-none", "standin-default-protocol"] {
        assert_eq!(
            modules[module_id]["program"], "crates/ck-bus/tests/support/standin_child.sh",
            "observable stand-in program path must name slice 1 support script"
        );
    }
    let env = modules["ckbus"]["env"]
        .as_object()
        .expect("observable ckbus env must be an object");
    assert!(
        !env.contains_key("CKBUS_SENTINEL_PERIOD_MS"),
        "observable shipped declaration must omit sentinel period override"
    );
    assert!(
        !env.contains_key("CKBUS_SENTINEL_TIMEOUT_MS"),
        "observable shipped declaration must omit sentinel timeout override"
    );
}

#[test]
fn harness_report_refuses_unnamed_or_unadvertised_stub_results() {
    use harness::report::{Row, RowReport};
    let advertised: BTreeSet<String> = stubs::CLAUSTRUM_OPERATIONS
        .iter()
        .map(|op| (*op).to_string())
        .collect();
    RowReport::skipped(
        Row::InstallBootstrap,
        "stub-reply-shape-unrecorded",
        "claustrum credential.get has no recorded reply body",
    )
    .served_by_harness_stub("credential.get")
    .validate(&advertised)
    .expect("observable named harness-stub condition must validate");
    let error = RowReport::passed(Row::InstallBootstrap)
        .served_by_harness_stub("credential.unadvertised")
        .validate(&advertised)
        .expect_err("observable unadvertised stub operation must be refused");
    assert!(
        error.contains("unadvertised operation"),
        "observable report refusal must name the unadvertised operation"
    );
    let gate_error = RowReport::skipped(
        Row::Census,
        "spawn-stream-unlanded",
        "spawn operation absent",
    )
    .validate(&advertised)
    .expect_err("observable row must refuse a gate outside its Gates column");
    assert!(
        gate_error.contains("may not record gate"),
        "observable gate refusal must name the row/gate mismatch"
    );
}

#[tokio::test]
async fn runtime_defaults_refuse_by_area_name() {
    let credentials = runtime_seams::refusing_credential_minting()
        .reconcile_credentials()
        .await
        .expect_err("observable credential seam default must refuse");
    assert_eq!(
        credentials.area(),
        "credential-minting",
        "observable credential seam refusal must name its area"
    );

    let census = runtime_seams::refusing_census();
    assert_eq!(
        census
            .read_census()
            .await
            .expect_err("observable census read default must refuse")
            .area(),
        "census",
        "observable census read refusal must name its area"
    );
    assert_eq!(
        census
            .write_census()
            .await
            .expect_err("observable census write default must refuse")
            .area(),
        "census",
        "observable census write refusal must name its area"
    );

    assert_eq!(
        runtime_seams::refusing_revocation()
            .resume_revocations()
            .await
            .expect_err("observable revocation default must refuse")
            .area(),
        "revocation",
        "observable revocation refusal must name its area"
    );
    let account = cortexkit_bus_naming::AccountNames::derive("box_declaration")
        .expect("an in-lexicon account derives");
    match runtime_seams::refusing_grant_generation()
        .own_user_grant(runtime_seams::OwnUser::BusModule, &account, "ckbus")
        .expect_err("observable grant default must refuse")
    {
        runtime_seams::GrantSeamError::NotLanded(refusal) => assert_eq!(
            refusal.area(),
            "grant-generation",
            "observable grant refusal must name its area"
        ),
        other => panic!("the grant default must refuse as not landed, got {other:?}"),
    }
    assert_eq!(
        runtime_seams::refusing_spawn_stream()
            .consume_spawn_stream()
            .await
            .expect_err("observable spawn stream default must refuse")
            .area(),
        "spawn-stream",
        "observable spawn-stream refusal must name its area"
    );
    assert_eq!(
        runtime_seams::refusing_leaf_configuration()
            .refresh_leaf_configuration()
            .await
            .expect_err("observable leaf default must refuse")
            .area(),
        "leaf-configuration",
        "observable leaf refusal must name its area"
    );
    assert_eq!(
        runtime_seams::refusing_sentinel_health()
            .report_health()
            .await
            .expect_err("observable sentinel-health default must refuse")
            .area(),
        "sentinel-health",
        "observable sentinel-health refusal must name its area"
    );
}

async fn wait_for_live_supervisor_entry(run: &AcceptanceRun) -> subc_control::SupervisorEntry {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = control::response(
            &run.connection_file,
            ClientControlRequest::SupervisorList {},
        )
        .await;
        let ClientControlResponse::SupervisorList { modules, .. } = response else {
            panic!("observable supervisor.list must return its matching response variant");
        };
        if let Some(entry) = modules
            .into_iter()
            .find(|entry| entry.module_id == MODULE_ID && entry.live)
        {
            return entry;
        }
        assert!(
            Instant::now() < deadline,
            "observable supervisor.list ckbus entry did not become live"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_narrowed_provenance_pid(run: &AcceptanceRun) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let response = control::response(
            &run.connection_file,
            ClientControlRequest::SupervisorProvenance {
                module_id: Some(MODULE_ID.to_string()),
            },
        )
        .await;
        let ClientControlResponse::SupervisorProvenance { modules, .. } = response else {
            panic!("observable supervisor.provenance must return its matching response variant");
        };
        let matching: Vec<_> = modules
            .into_iter()
            .filter(|entry| entry.module_id == MODULE_ID)
            .collect();
        assert_eq!(matching.len(), 1, "observable narrowed supervisor.provenance must contain exactly one matching ckbus entry");
        if let Some(pid) = matching[0].daemon_observed.pid {
            return pid;
        }
        assert!(Instant::now() < deadline, "observable supervisor.provenance ckbus daemon_observed.pid stayed absent for 2 seconds");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn catalog_entries(run: &AcceptanceRun, module_id: &str) -> Vec<subc_control::CatalogEntry> {
    let response = control::response(
        &run.connection_file,
        ClientControlRequest::CatalogList {
            module_id: Some(module_id.to_string()),
        },
    )
    .await;
    let ClientControlResponse::CatalogList { modules, .. } = response else {
        panic!("observable catalog.list must return its matching response variant");
    };
    modules
        .into_iter()
        .filter(|entry| entry.module_id == module_id)
        .collect()
}

async fn assert_stub_catalog(run: &AcceptanceRun, module_id: &str, expected: &[&str]) {
    let entries = catalog_entries(run, module_id).await;
    assert_eq!(
        entries.len(),
        1,
        "observable harness stub {module_id} must register exactly once"
    );
    let actual = stubs::operation_names(&entries[0]);
    let expected: BTreeSet<String> = expected.iter().map(|op| (*op).to_string()).collect();
    assert_eq!(
        actual, expected,
        "observable harness stub {module_id} advertised vocabulary must match its fire-time record"
    );
}

async fn assert_stub_principal_controls(run: &AcceptanceRun) {
    let request = |consumer_identity| ClientControlRequest::RouteOpen {
        // Claustrum registers its read surface as a management surface, and so does
        // the stub standing in for it.
        target: RouteTarget::ManagementSurface {
            module_id: "claustrum".to_string(),
        },
        identity: BindIdentity::new(&*run.root, "ck-bus-acceptance", "principal-control"),
        consumer_identity,
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope: None,
    };

    let direct = control::rpc(&run.connection_file, request(None)).await;
    let control::ControlReply::Error(direct_error) = direct else {
        panic!("observable claim-absent route must be refused by the harness stub");
    };
    assert_eq!(
        direct_error.code, "harness_stub_principal_refused",
        "observable claim-absent route refusal must be attributed to the harness stub"
    );
    assert_eq!(
        run.claustrum.observed_principals(),
        vec![Some(Principal::Direct)],
        "observable claim-absent route must reach the stub with daemon-stamped Direct principal"
    );

    let altered = control::rpc(
        &run.connection_file,
        request(Some(ConsumerIdentity {
            module_id: MODULE_ID.to_string(),
            launch_nonce: "altered-launch-nonce".to_string(),
        })),
    )
    .await;
    let control::ControlReply::Error(altered_error) = altered else {
        panic!("observable altered launch claim must be refused by the daemon");
    };
    assert_eq!(
        altered_error.code, "bad_consumer_identity",
        "observable altered launch claim refusal must name bad_consumer_identity"
    );
    assert_eq!(
        run.claustrum.observed_principals(),
        vec![Some(Principal::Direct)],
        "observable altered launch claim must not reach the harness stub"
    );
}

async fn assert_hand_started_copy_refused(run: &AcceptanceRun, binary: &Path, nonce: Option<&str>) {
    // The same `ckdev-bus` placement the run's daemon supervises, so the copy
    // differs from the supervised module only in who started it.
    let binary = cortexkit_test_support::ckdev_binary(binary);
    let mut command = Command::from(cortexkit_test_support::dev_command(binary));
    command
        .arg("--subc")
        .arg(&run.connection_file)
        .env("SUBC_MODULE_ID", MODULE_ID)
        .env("XDG_DATA_HOME", run.root.join("data"))
        .env("XDG_RUNTIME_DIR", run.root.join("run"))
        .env("XDG_CONFIG_HOME", run.root.join("config"))
        // This is a manual negative control, not a supervised spawn. The SDK
        // still accepts an environment nonce when no descriptor is named.
        .env_remove("SUBC_LAUNCH_NONCE_FD")
        .env_remove("SUBC_LAUNCH_NONCE");
    if let Some(nonce) = nonce {
        command.env("SUBC_LAUNCH_NONCE", nonce);
    }
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .expect("observable hand-started copy must be refused without hanging")
        .expect("observable hand-started copy must execute");
    assert!(
        !output.status.success(),
        "observable hand-started ck-bus HELLO must be refused for nonce {:?}",
        nonce
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HelloRejected") && stderr.contains("code: \"reserved_module\""),
        "observable hand-started connection must receive the reserved_module HELLO refusal, stderr={stderr:?}"
    );
}

fn assert_reserved_none_declaration_is_refused(valid_config: &Path) {
    let mut value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(valid_config).expect("observable rendered config must be readable"),
    )
    .expect("observable rendered config must decode");
    value["modules"][MODULE_ID]["protocol"] = serde_json::Value::String("none".to_string());
    let invalid = valid_config.with_file_name("reserved-none.json");
    std::fs::write(&invalid, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let error = subc_daemon::daemon_config::load(&invalid)
        .expect_err("observable reserved true plus protocol none must be refused");
    let message = error.to_string();
    assert!(
        message.contains("reserved: true") && message.contains("protocol: \"none\""),
        "observable parse refusal must name reserved true and protocol none: {message}"
    );
}

async fn record_server_describe(run: &AcceptanceRun) {
    let response = control::response(
        &run.connection_file,
        ClientControlRequest::ServerDescribe {},
    )
    .await;
    let ClientControlResponse::ServerDescribe { build_git_sha, .. } = response else {
        panic!("observable server.describe must return its matching response variant");
    };
    eprintln!("fire-time server.describe build_git_sha={build_git_sha:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_ckbus_hello_declares_build_provenance() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let run = AcceptanceRun::start(Path::new(env!("CARGO_BIN_EXE_ck-bus"))).await;
    run.wait_for_catalog_id(MODULE_ID).await;
    let response = control::response(
        &run.connection_file,
        ClientControlRequest::SupervisorProvenance {
            module_id: Some(MODULE_ID.to_string()),
        },
    )
    .await;
    let ClientControlResponse::SupervisorProvenance { modules, .. } = response else {
        panic!("expected supervisor provenance response");
    };
    assert_eq!(modules.len(), 1);
    let subc_control::ModuleDeclaredProvenance::Reported { build } = &modules[0].module_declared
    else {
        panic!("real ckbus HELLO omitted build provenance");
    };
    assert_eq!(
        build.wire_crate_version.as_deref(),
        Some(subc_protocol::SUBC_PROTOCOL_CRATE_VERSION)
    );
    let sha = build
        .build_git_sha
        .as_deref()
        .expect("real HELLO must carry build git SHA");
    assert_eq!(sha.len(), 40);
    assert!(sha.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(build.build_lock_digest.as_ref().unwrap().len(), 64);
    assert_eq!(
        build.launch_nonce_source,
        Some(expected_launch_nonce_source())
    );
    run.shutdown().await;
}

/// Where a supervised module gets its launch nonce on this platform: the daemon
/// hands it over on an inherited Unix pipe or a PID-authenticated Windows pipe.
fn expected_launch_nonce_source() -> subc_protocol::manifest::LaunchNonceSource {
    if cfg!(windows) {
        subc_protocol::manifest::LaunchNonceSource::Pipe
    } else {
        subc_protocol::manifest::LaunchNonceSource::Fd
    }
}

/// Another process can see the descriptor name, but not the secret it carries.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supervised_nonce_cannot_be_scraped_from_the_process_environment() {
    let _gate = harness::acceptance_gate().await;
    let run = AcceptanceRun::start(Path::new(env!("CARGO_BIN_EXE_ck-bus"))).await;
    let pid = wait_for_narrowed_provenance_pid(&run).await;
    let environment = harness::signer::seeds::process_environment(pid);
    assert!(
        harness::signer::seeds::environment_value(&environment, "SUBC_LAUNCH_NONCE_FD")
            .is_some_and(|value| value.starts_with("3:")),
        "missing descriptor name: {environment}"
    );
    assert!(
        harness::signer::seeds::environment_value(&environment, "SUBC_LAUNCH_NONCE").is_none(),
        "a process-environment scrape must not reveal the launch secret"
    );
    run.shutdown().await;
}
