use super::*;
use crate::caller_identity::CallerIdentity;
use crate::config_store::StoreRoot;
use serde_json::json;
use subc_protocol::config::ConfigGetRequest;

#[path = "daemon_readers_tests.rs"]
mod daemon_readers;
#[path = "status_placement_tests.rs"]
mod status_placement;

struct Rig {
    path: std::path::PathBuf,
    declaration: ConfigDeclareRequest,
    #[cfg(unix)]
    program_mode: std::sync::OnceLock<&'static str>,
}

#[test]
fn config_recovery_plan_does_not_validate_a_separate_pre_capture_read() {
    let rig = Rig::new();
    let owner = rig.owner("single-captured-daemon");
    let path = rig.path.join("subc.jsonc");
    std::fs::write(
        &path,
        br#"{"version":1,"modules":{},"port":"invalid-port"}"#,
    )
    .unwrap();
    let captured = br#"{"version":1,"modules":{}}"#;
    *owner.prepare_capture_hook.lock().unwrap() = Some(Box::new(move || {
        std::fs::write(path, captured).unwrap();
    }));
    let prepared = owner
        .prepare(PlanKind::Stamp)
        .expect("only the captured daemon bytes are authoritative for the plan");
    assert_eq!(
        prepared.candidate.daemon.bytes.as_deref(),
        Some(captured.as_slice())
    );
}

#[test]
fn config_recovery_plan_validates_the_exact_captured_daemon_bytes() {
    let rig = Rig::new();
    let owner = rig.owner("captured-daemon-validation");
    let path = rig.path.join("subc.jsonc");
    *owner.prepare_capture_hook.lock().unwrap() = Some(Box::new(move || {
        std::fs::write(path, br#"{"version":1,"modules":{},"port":"invalid-port"}"#).unwrap();
    }));
    let error = owner
        .prepare(PlanKind::Stamp)
        .err()
        .expect("an invalid captured daemon config must not produce a plan");
    assert_eq!(error.code, "invalid_daemon_config");
}

#[test]
fn config_recovery_get_keeps_only_active_memory_and_refuses_declare() {
    let rig = Rig::new();
    let raw = br#"{"credential":{"vault_ref":"approved"},"limit":2}"#;
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), raw).unwrap();
    let owner = rig.owner("recovery-active");
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
    let store = rig.path.join("store").join(crate::config_store::STORE_FILE);
    std::fs::write(&store, b"tampered").unwrap();
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), br#"{"limit":99}"#).unwrap();
    assert!(owner.check_recovery().unwrap());
    let reply = owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap();
    assert!(reply.pending.is_none());
    assert_eq!(
        reply.active.unwrap().layers[0].bytes.as_deref(),
        Some(raw.as_slice())
    );
    assert_eq!(
        owner
            .declare(ConnectionId::new(11), "a", rig.declaration.clone(), true)
            .unwrap_err()
            .code,
        "config_gated"
    );
    assert_eq!(std::fs::read(store).unwrap(), b"tampered");
}

#[test]
fn config_recovery_exit_rejects_invalid_or_missing_daemon_config_without_writes() {
    let rig = Rig::new();
    let owner = rig.owner("recovery-invalid");
    rig.stamp(&owner);
    let store = rig.path.join("store").join(crate::config_store::STORE_FILE);
    std::fs::write(&store, b"tampered").unwrap();
    assert!(owner.check_recovery().unwrap());
    for invalid in [Some(b"{ invalid".as_slice()), None] {
        if let Some(bytes) = invalid {
            std::fs::write(rig.path.join("subc.jsonc"), bytes).unwrap();
        } else {
            std::fs::remove_file(rig.path.join("subc.jsonc")).unwrap();
        }
        for kind in [PlanKind::Stamp, PlanKind::Unstamp] {
            assert_eq!(
                owner.prepare(kind).err().unwrap().code,
                "invalid_daemon_config"
            );
            assert_eq!(std::fs::read(&store).unwrap(), b"tampered");
            assert!(owner.is_recovering());
        }
    }
}

#[tokio::test]
async fn config_recovery_allowlist_precedes_probed_and_legacy_ops() {
    let rig = Rig::new();
    let owner = std::sync::Arc::new(rig.owner("recovery-ops"));
    rig.stamp(&owner);
    let store = rig.path.join("store").join(crate::config_store::STORE_FILE);
    std::fs::write(&store, b"tampered").unwrap();
    let handler = crate::ControlHandler::new(std::sync::Arc::new(crate::Registry::default()))
        .with_config_runtime(owner.clone())
        .with_privileged_operator_prompt(crate::daemon_config::PrivilegedOperatorPrompt::Enforce);
    let (sender, _receiver) = tokio::sync::mpsc::channel(16);
    let sink = crate::FrameSink::new(sender);
    let ctx = crate::RouteCtx {
        connection_id: ConnectionId::new(50),
        egress: sink,
    };
    for op in [
        "config.plan",
        "supervisor.set_enabled",
        "supervisor.rescan",
        "supervisor.rescan_checked",
        "supervisor.restart",
        "supervisor.reload",
        "supervisor.swap",
        "route.open",
        "catalog.list",
        "operator.confirm",
        "scope.served_by",
        "activity.record",
        "config.declare",
        "config.applied",
        "unknown.op",
    ] {
        let frame = subc_protocol::Frame::build(
            subc_protocol::FrameType::Request,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&json!({"op":op})).unwrap(),
        )
        .unwrap();
        let reply = handler.handle_control_frame(&ctx, frame).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reply[0].body).unwrap();
        assert_eq!(body["code"], "config_gated", "{op}: {body}");
        assert_eq!(std::fs::read(&store).unwrap(), b"tampered", "{op}");
    }
    assert_eq!(owner.status().unwrap().state, "store_unverified");
    assert_eq!(std::fs::read(store).unwrap(), b"tampered");
}

#[cfg(windows)]
#[tokio::test]
async fn config_admission_windows_keeps_covered_file_write_exclusion_until_hello() {
    use crate::supervise::{RestartPolicy, Supervisor, SupervisorHandle};
    use std::sync::Arc;
    let rig = Rig::new();
    let program = rig.path.join("program.exe");
    std::fs::copy(
        std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/ping.exe"),
        &program,
    )
    .unwrap();
    let covered = rig.path.join("covered.txt");
    std::fs::write(&covered, b"approved").unwrap();
    std::fs::write(rig.path.join("subc.jsonc"),serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":program,"args":["-t","127.0.0.1"],"protocol":"subc","registration_channel":true,"overlap":"safe","program_files":[covered]}}})).unwrap()).unwrap();
    let owner = Arc::new(rig.owner("windows-handle-lifetime"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let handle = SupervisorHandle::new();
    handle.capture_registration_channels_for_test();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone())
        .with_forwarding(forwarding.clone())
        .with_drain_timeout(std::time::Duration::from_millis(20));
    let spec = crate::daemon_config::load(rig.path.join("subc.jsonc"))
        .unwrap()
        .unwrap()
        .modules[0]
        .module_spec();
    let rejected_module = supervisor.spawn_async(spec.clone()).await.unwrap();
    let before = std::fs::OpenOptions::new().write(true).open(&covered);
    let excluded = before.as_ref().err().and_then(|error| error.raw_os_error());
    drop(before);
    let rejected_proof = handle.registration_proof_for_test("a").unwrap();
    handle
        .registration_for_test("a")
        .unwrap()
        .wait_launch()
        .await;
    let handler = crate::ControlHandler::with_forwarding(registry.clone(), forwarding)
        .with_supervisor(handle.clone())
        .with_config_runtime(owner);
    let mut manifest = subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build();
    manifest.ready = Some(true);
    let hello = subc_protocol::ModuleHelloBody {
        manifest,
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: rejected_proof.nonce(),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    let mut malformed = frame.clone();
    let mut invalid: serde_json::Value = serde_json::from_slice(&malformed.body).unwrap();
    invalid["protocol_ver"] = json!(0);
    malformed.body = serde_json::to_vec(&invalid).unwrap();
    rejected_proof.bind(ConnectionId::new(778));
    let (rejected_tx, mut rejected_rx) = tokio::sync::mpsc::channel(8);
    let rejected = handler
        .handle_control_frame(
            &crate::router::RouteCtx {
                connection_id: ConnectionId::new(778),
                egress: crate::FrameSink::new(rejected_tx),
            },
            malformed,
        )
        .await
        .unwrap();
    let after_rejection = std::fs::OpenOptions::new().write(true).open(&covered);
    let still_excluded = after_rejection
        .as_ref()
        .err()
        .and_then(|error| error.raw_os_error());
    drop(after_rejection);
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].header.ty, subc_protocol::FrameType::Error);
    let rejection: serde_json::Value = serde_json::from_slice(&rejected[0].body).unwrap();
    assert_eq!(rejection["code"], "invalid_manifest");
    assert!(rejection["message"]
        .as_str()
        .unwrap()
        .starts_with("HELLO protocol_ver 0 does not match manifest protocol_ver"));
    handler.cleanup_connection(ConnectionId::new(778)).unwrap();
    rejected_module.stop().await.unwrap();
    let module = supervisor.spawn_async(spec).await.unwrap();
    let proof = handle.registration_proof_for_test("a").unwrap();
    handle
        .registration_for_test("a")
        .unwrap()
        .wait_launch()
        .await;
    let ordinary_excluded = std::fs::OpenOptions::new()
        .write(true)
        .open(&covered)
        .err()
        .and_then(|error| error.raw_os_error());
    let connection = ConnectionId::new(777);
    proof.bind(connection);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let reply = handler
        .handle_control_frame(
            &crate::router::RouteCtx {
                connection_id: connection,
                egress: crate::FrameSink::new(tx),
            },
            frame,
        )
        .await
        .unwrap();
    // A refused HELLO enqueues no ACK and returns its refusal as the reply, so
    // name that refusal instead of the bare empty-channel error.
    let ack = rx.try_recv().unwrap_or_else(|error| {
        let replies: Vec<_> = reply
            .iter()
            .map(|frame| String::from_utf8_lossy(&frame.body).into_owned())
            .collect();
        panic!("HELLO was not acknowledged ({error:?}); the handler replied {replies:?}")
    });
    let after = std::fs::OpenOptions::new().write(true).open(&covered);
    let released = after.is_ok();
    drop(after);
    let swapping = {
        let module = module.clone();
        tokio::spawn(async move { module.swap(Some(std::time::Duration::from_secs(5))).await })
    };
    let candidate_proof = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(proof) = handle.candidate_registration_proof_for_test("a") {
                if handle.has_live_verified_program_binding_for_test("a", &proof)
                    && handle.wait_program_binding_for_test("a", &proof).await
                {
                    break proof;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let before_candidate_ack = std::fs::OpenOptions::new().write(true).open(&covered);
    let candidate_excluded = before_candidate_ack
        .as_ref()
        .err()
        .and_then(|error| error.raw_os_error());
    drop(before_candidate_ack);
    let mut candidate_hello = hello;
    candidate_hello.launch_nonce = candidate_proof.nonce();
    let candidate_frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        2,
        serde_json::to_vec(&candidate_hello).unwrap(),
    )
    .unwrap();
    let candidate_connection = ConnectionId::new(779);
    candidate_proof.bind(candidate_connection);
    let (candidate_tx, mut candidate_rx) = tokio::sync::mpsc::channel(8);
    let candidate_reply = handler
        .handle_control_frame(
            &crate::router::RouteCtx {
                connection_id: candidate_connection,
                egress: crate::FrameSink::new(candidate_tx),
            },
            candidate_frame,
        )
        .await
        .unwrap();
    let candidate_ack = candidate_rx.try_recv().unwrap();
    let after_candidate_ack = std::fs::OpenOptions::new().write(true).open(&covered);
    let candidate_released = after_candidate_ack.is_ok();
    drop(after_candidate_ack);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while registry
            .get_module("a")
            .unwrap()
            .is_none_or(|registration| registration.connection_id != candidate_connection)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    handler.cleanup_connection(connection).unwrap();
    let outcome = swapping.await.unwrap();
    handler.cleanup_connection(candidate_connection).unwrap();
    module.stop().await.unwrap();
    assert_eq!(
        excluded,
        Some(32),
        "covered file must keep its no-write-share witness after resume and before HELLO"
    );
    assert_eq!(
        still_excluded,
        Some(32),
        "a refused HELLO must not release the child's write-exclusion witness"
    );
    assert!(rejected_rx.try_recv().is_err());
    assert!(
        reply.is_empty(),
        "production forwarding queues the ACK rather than returning it"
    );
    assert_eq!(ack.frame.header.ty, subc_protocol::FrameType::HelloAck);
    assert_eq!(ordinary_excluded, Some(32));
    assert!(
        released,
        "successful HELLO releases the Windows write-exclusion handles"
    );
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(candidate_excluded, Some(32));
    assert!(candidate_reply.is_empty());
    assert_eq!(
        candidate_ack.frame.header.ty,
        subc_protocol::FrameType::HelloAck
    );
    assert!(
        candidate_released,
        "a production swap-candidate ACK must release its strict handles too"
    );
}

#[cfg(unix)]
#[path = "admission_tests.rs"]
mod admission_tests;
#[path = "audit_authority_tests.rs"]
mod authority_tests;
#[path = "delivery_tests.rs"]
mod delivery_tests;

fn desktop_plan() -> (Rig, RuntimeOwner, ConfigPlanView) {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"safe-reference"}}"#,
    )
    .unwrap();
    let owner = rig.owner("desktop-view");
    rig.declare(&owner, 10);
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    (rig, owner, plan)
}

#[test]
fn config_desktop_owner_requires_exact_full_view_on_the_same_connection() {
    let (_rig, owner, plan) = desktop_plan();
    let refused = || {
        owner
            .begin_approval(&plan.digest, &Rig::facts())
            .unwrap_err()
            .code
    };
    assert_eq!(refused(), "approval_surface_unavailable");
    let mut forged = plan.clone();
    forged.plan.changeset.apply_plan = json!({"omitted_effects":true});
    assert_eq!(
        owner
            .desktop_view_delivered(ConnectionId::new(7), &forged)
            .unwrap_err()
            .code,
        "plan_stale"
    );
    assert_eq!(refused(), "approval_surface_unavailable");
    owner
        .desktop_view_delivered(ConnectionId::new(8), &plan)
        .unwrap();
    assert_eq!(refused(), "approval_surface_unavailable");
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    assert!(matches!(
        owner.begin_approval(&plan.digest, &Rig::facts()).unwrap(),
        ApprovalChallenge::Desktop { .. }
    ));
}

#[test]
fn config_desktop_owner_rechecks_replaced_view_after_presence() {
    let (_rig, owner, plan) = desktop_plan();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let attempt = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    let other = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), |old, new| {
            let mut review = daemon_review(old, new)?;
            review
                .policy_changes
                .push(json!({"reviewed_variant":"second plan"}));
            Ok(review)
        })
        .unwrap();
    assert_ne!(other.digest, plan.digest);
    owner
        .desktop_view_delivered(ConnectionId::new(7), &other)
        .unwrap();
    assert_eq!(
        owner
            .finish_approval(
                attempt.attempt(),
                &Rig::facts(),
                ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
                1
            )
            .unwrap_err()
            .code,
        "approval_surface_unavailable"
    );
    assert_eq!(owner.status().unwrap().state, "unstamped");
    assert!(owner.history().unwrap().0.is_empty());
    // A failed attempt leaves the old immutable plan reviewable, not approved.
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    assert!(owner.begin_approval(&plan.digest, &Rig::facts()).is_ok());
}

#[test]
fn config_desktop_owner_disconnect_expiry_and_consumption_clear_full_view_receipts() {
    let (_rig, owner, plan) = desktop_plan();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let attempt = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner.disconnected(ConnectionId::new(7));
    assert_eq!(
        owner
            .finish_approval(
                attempt.attempt(),
                &Rig::facts(),
                ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
                1
            )
            .unwrap_err()
            .code,
        "approval_surface_unavailable"
    );
    assert_eq!(
        owner
            .begin_approval(&plan.digest, &Rig::facts())
            .unwrap_err()
            .code,
        "approval_surface_unavailable"
    );
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    {
        let mut state = owner.inner.lock().unwrap();
        let deadline = state.plan_deadlines[&plan.digest];
        state.expire_idle(deadline);
        assert!(state.desktop_views.is_empty());
    }
    let replacement = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &replacement)
        .unwrap();
    let attempt = owner
        .begin_approval(&replacement.digest, &Rig::facts())
        .unwrap();
    owner
        .finish_approval(
            attempt.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
            1,
        )
        .unwrap();
    assert!(owner.inner.lock().unwrap().desktop_views.is_empty());
}
impl Rig {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(windows)]
        let root = std::fs::canonicalize(root).unwrap();
        let path = root.join(format!(
            "subc-owner-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path)
            .or_else(|error| {
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
                // A dead test process may have left this PID/counter directory.
                // Recreate it exclusively instead of reusing its old contents.
                std::fs::remove_dir_all(&path)?;
                std::fs::create_dir(&path)
            })
            .unwrap();
        std::fs::create_dir(path.join("cortexkit")).unwrap();
        std::fs::create_dir(path.join("store")).unwrap();
        std::fs::write(path.join("program"), b"captured program identity").unwrap();
        std::fs::write(
            path.join("subc.jsonc"),
            serde_json::to_vec(
                &json!({"version":1,"modules":{"a":{"program":path.join("program"),"registration_channel":true}}}),
            )
            .unwrap(),
        )
        .unwrap();
        let schema = json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
            "unevaluatedProperties":false, "x-ck-layers":["user"], "x-ck-apply":"live",
            "properties": {
                "credential":{"type":"object","description":"Vault credential","x-ck-kind":"credential_ref","x-ck-required":true},
                "limit":{"type":"integer","description":"Limit","default":1}
            }
        });
        let declaration = ConfigDeclareRequest::new(
            schema,
            path.join("cortexkit/a.jsonc").to_string_lossy(),
            vec![],
        );
        Self {
            path,
            declaration,
            #[cfg(unix)]
            program_mode: std::sync::OnceLock::new(),
        }
    }

    fn owner(&self, incarnation: &str) -> RuntimeOwner {
        RuntimeOwner::new(
            incarnation.into(),
            ConfigStore::open_digest_only_for_tests(
                StoreRoot::from_service_definition(self.path.join("store")).unwrap(),
            ),
            ConfigCapturer {
                daemon_path: self.path.join("subc.jsonc"),
                config_home: self.path.clone(),
                projects: std::sync::Arc::new(super::super::capture::UnavailableProjectRegistry),
            },
        )
    }

    fn declare(&self, owner: &RuntimeOwner, connection: u64) {
        owner
            .declare(
                ConnectionId::new(connection),
                "a",
                self.declaration.clone(),
                true,
            )
            .unwrap();
    }

    fn facts() -> ApprovalFacts {
        ApprovalFacts {
            connection: 7,
            owner_uid: true,
            desktop_app: true,
            attested_operator_app: true,
            os_prompt_available: true,
            phone: None,
            caller: CallerIdentity::default(),
        }
    }

    #[cfg(unix)]
    fn write_native_program(&self, mode: &'static str) {
        // Cargo builds the native fixture alongside the test binary; no shell
        // wrapper or runtime compiler is needed for gated launch fixtures.
        let binary = crate::config_gate::program::native_fixture_for_test();
        std::fs::copy(binary, self.path.join("program")).unwrap();
        self.program_mode
            .set(mode)
            .expect("fixture mode already set");
        #[cfg(target_os = "macos")]
        assert!(std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(self.path.join("program"))
            .status()
            .unwrap()
            .success());
    }

    fn stamp(&self, owner: &RuntimeOwner) -> u64 {
        #[cfg(unix)]
        if let Some(mode) = self.program_mode.get() {
            let path = self.path.join("subc.jsonc");
            let mut doc: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            for entry in doc["modules"].as_object_mut().unwrap().values_mut() {
                if entry["program"] == json!(self.path.join("program")) {
                    let args = entry
                        .as_object_mut()
                        .unwrap()
                        .entry("args")
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .unwrap();
                    if args.first() != Some(&json!(mode)) {
                        args.insert(0, json!(mode));
                    }
                }
            }
            std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
        }
        let prepared = owner.prepare(PlanKind::Stamp).unwrap();
        let plan = owner.finish_plan(prepared, daemon_review).unwrap();
        owner
            .desktop_view_delivered(ConnectionId::new(7), &plan)
            .unwrap();
        let challenge = owner.begin_approval(&plan.digest, &Self::facts()).unwrap();
        owner
            .finish_approval(
                challenge.attempt(),
                &Self::facts(),
                ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
                1,
            )
            .unwrap()
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn config_owner_raw_credential_reference_survives_commit_restart_and_get_byte_exact() {
    let rig = Rig::new();
    let raw = b"{ // approved reference, not a secret\r\n\"credential\":{\"vault_ref\":\"vault-reference\"}, \"limit\":2,}\r\n";
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), raw).unwrap();
    let owner = rig.owner("first");
    rig.declare(&owner, 10);
    let path = rig.path.join("subc.jsonc");
    let declared = std::fs::read(&path).unwrap();
    let mut missing: serde_json::Value = serde_json::from_slice(&declared).unwrap();
    missing["modules"]["a"]
        .as_object_mut()
        .unwrap()
        .remove("registration_channel");
    std::fs::write(&path, serde_json::to_vec(&missing).unwrap()).unwrap();
    let refused = owner
        .prepare(PlanKind::Stamp)
        .err()
        .expect("an enabled entry must declare channel support");
    assert_eq!(refused.code, subc_control::REGISTRATION_CHANNEL_UNSUPPORTED);
    assert!(refused.detail.ends_with(": a"), "{}", refused.detail);
    std::fs::write(path, declared).unwrap();
    assert_eq!(rig.stamp(&owner), 1);
    let reply = owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(
        reply.pending.as_ref().unwrap().layers[0].bytes.as_deref(),
        Some(raw.as_slice())
    );
    drop(owner);

    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"unapproved-reference"},"limit":99}"#,
    )
    .unwrap();
    let restarted = rig.owner("second");
    rig.declare(&restarted, 20);
    let reply = restarted
        .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(reply.pending.as_ref().unwrap().generation, 1);
    assert_eq!(
        reply.pending.as_ref().unwrap().layers[0].bytes.as_deref(),
        Some(raw.as_slice())
    );
    assert!(!serde_json::to_string(&restarted.history().unwrap())
        .unwrap()
        .contains("vault-reference"));
}

#[test]
fn config_owner_rollback_retains_raw_layers_and_recaptures_the_restored_program_path() {
    let rig = Rig::new();
    let raw = b"{ // saved rollback input\r\n\"credential\":{\"vault_ref\":\"old-reference\"},\"limit\":2,}\r\n";
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), raw).unwrap();
    let owner = rig.owner("rollback");
    rig.declare(&owner, 10);
    assert_eq!(rig.stamp(&owner), 1);
    let recorded = owner
        .inner
        .lock()
        .unwrap()
        .store
        .rollback_configuration(1)
        .unwrap();
    let recorded = super::super::GateSnapshot::from_store(&recorded).unwrap();
    std::fs::write(
        rig.path.join("program"),
        b"current replacement at restored path",
    )
    .unwrap();
    let current_daemon = serde_json::to_vec(
        &json!({"version":1,"modules":{"a":{"program":rig.path.join("program"),"args":["--proposal"]}}}),
    )
    .unwrap();
    std::fs::write(rig.path.join("subc.jsonc"), &current_daemon).unwrap();
    let current_layer = br#"{"credential":{"vault_ref":"unapproved-reference"},"limit":99}"#;
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), current_layer).unwrap();
    let prepared = owner.prepare(PlanKind::Rollback(1)).unwrap();
    let key = super::super::InputKey::Program {
        module: "a".into(),
        path: rig.path.join("program").to_string_lossy().into_owned(),
    };
    let current_identity = prepared.candidate.diagnostic.inputs[&key].clone();
    assert_ne!(current_identity, recorded.inputs[&key]);
    assert_eq!(
        prepared.candidate.modules["a"].view.layers[0]
            .bytes
            .as_deref(),
        Some(raw.as_slice())
    );
    let plan = owner.finish_plan(prepared, daemon_review).unwrap();
    let difference = &plan.plan.changeset.apply_plan["rollback_programs"][0];
    assert_eq!(difference["reason"], "program changed since that version");
    assert_eq!(
        difference["approved_then"],
        serde_json::to_value(&recorded.inputs[&key]).unwrap()
    );
    assert_eq!(
        difference["on_disk_now"],
        serde_json::to_value(&current_identity).unwrap()
    );
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    assert_eq!(
        owner
            .finish_approval(
                challenge.attempt(),
                &Rig::facts(),
                ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
                2
            )
            .unwrap(),
        2
    );
    let snapshot = owner.approved_snapshot().unwrap();
    assert_eq!(
        snapshot.modules["a"].layers[0].bytes.as_deref(),
        Some(raw.as_slice())
    );
    let restored_daemon = snapshot.daemon.as_ref().unwrap().bytes.as_ref().unwrap();
    let doc = crate::daemon_config::parse_with_activity(
        std::str::from_utf8(restored_daemon).unwrap(),
        rig.path.join("subc.jsonc"),
    );
    assert!(doc.is_ok());
    let daemon_doc: serde_json::Value = serde_json::from_slice(restored_daemon).unwrap();
    assert_eq!(
        daemon_doc["modules"]["a"]["program"],
        rig.path.join("program").to_string_lossy().as_ref()
    );
    assert_eq!(
        std::fs::read(rig.path.join("subc.jsonc")).unwrap(),
        current_daemon
    );
    assert_eq!(
        std::fs::read(rig.path.join("cortexkit/a.jsonc")).unwrap(),
        current_layer
    );
    assert!(owner
        .inner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .proposal
        .is_some());
    let commands = owner.approved_rollback_commands(2).unwrap();
    assert_eq!(commands.generation, 2);
    assert_eq!(commands.daemon.bytes, snapshot.daemon.unwrap().bytes);
    assert_eq!(
        commands.programs["a"].path,
        rig.path.join("program").to_string_lossy()
    );
    assert_eq!(commands.programs["a"].identity, current_identity);
}

#[test]
fn config_admission_rollback_path_change_selects_the_restored_command() {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"approved-reference"}}"#,
    )
    .unwrap();
    let owner = rig.owner("rollback-command");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    std::fs::write(rig.path.join("other-program"), b"new configured program").unwrap();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(
            &json!({"version":1,"modules":{"a":{"program":rig.path.join("other-program")}}}),
        )
        .unwrap(),
    )
    .unwrap();
    let prepared = owner.prepare(PlanKind::Rollback(1)).unwrap();
    assert!(prepared
        .candidate
        .diagnostic
        .inputs
        .contains_key(&super::super::InputKey::Program {
            module: "a".into(),
            path: rig.path.join("program").to_string_lossy().into_owned(),
        }));
    assert_eq!(owner.history().unwrap().0.len(), 1);
}

#[test]
fn config_owner_rollback_freshness_reopens_layers_daemon_and_the_restored_program() {
    for changed in ["program", "cortexkit/a.jsonc", "subc.jsonc"] {
        let rig = Rig::new();
        std::fs::write(
            rig.path.join("cortexkit/a.jsonc"),
            br#"{"credential":{"vault_ref":"approved-reference"}}"#,
        )
        .unwrap();
        let owner = rig.owner("rollback-freshness");
        rig.declare(&owner, 10);
        rig.stamp(&owner);
        std::fs::write(rig.path.join("program"), b"replacement shown in rollback").unwrap();
        let prepared = owner.prepare(PlanKind::Rollback(1)).unwrap();
        let plan = owner.finish_plan(prepared, daemon_review).unwrap();
        owner
            .desktop_view_delivered(ConnectionId::new(7), &plan)
            .unwrap();
        let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
        let bytes: &[u8] = match changed {
            "program" => b"replaced after review",
            "cortexkit/a.jsonc" => {
                br#"{"credential":{"vault_ref":"approved-reference"},"limit":8}"#
            }
            _ => br#"{"version":1,"modules":{}}"#,
        };
        std::fs::write(rig.path.join(changed), bytes).unwrap();
        assert_eq!(
            owner
                .finish_approval(
                    challenge.attempt(),
                    &Rig::facts(),
                    ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
                    2
                )
                .unwrap_err()
                .code,
            "plan_stale",
            "{changed}"
        );
        assert_eq!(owner.history().unwrap().0.len(), 1);
    }
}

#[test]
fn config_owner_rollback_missing_program_is_named_and_creates_no_generation() {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"approved-reference"}}"#,
    )
    .unwrap();
    let owner = rig.owner("rollback-missing");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    std::fs::remove_file(rig.path.join("program")).unwrap();
    assert_eq!(
        owner.prepare(PlanKind::Rollback(1)).err().unwrap().code,
        "program_refused"
    );
    assert_eq!(owner.history().unwrap().0.len(), 1);
}

#[test]
fn config_owner_rollback_absent_layer_stays_absent_in_delivery_after_restart() {
    let mut rig = Rig::new();
    rig.declaration.schema["properties"]
        .as_object_mut()
        .unwrap()
        .remove("credential");
    let owner = rig.owner("rollback-absence");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    std::fs::write(rig.path.join("program"), b"replacement under the same path").unwrap();
    let proposal = br#"{"limit":8}"#;
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), proposal).unwrap();
    let prepared = owner.prepare(PlanKind::Rollback(1)).unwrap();
    assert_eq!(prepared.candidate.modules["a"].view.layers[0].bytes, None);
    let plan = owner.finish_plan(prepared, daemon_review).unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap();
    drop(owner);
    let reopened = rig.owner("rollback-absence-reopened");
    let commands = reopened.approved_rollback_commands(2).unwrap();
    assert_eq!(commands.generation, 2);
    let saved = reopened.approved_snapshot().unwrap();
    assert_eq!(saved.modules["a"].layers[0].bytes, None);
    assert_eq!(saved.modules["a"].layers[0].digest, None);
    assert_eq!(
        std::fs::read(rig.path.join("cortexkit/a.jsonc")).unwrap(),
        proposal
    );
}

#[test]
fn config_owner_requires_declaration_once_before_get() {
    let rig = Rig::new();
    let owner = rig.owner("declaration");
    assert_eq!(
        owner
            .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "declaration_order"
    );
    rig.declare(&owner, 10);
    assert_eq!(
        owner
            .declare(ConnectionId::new(10), "a", rig.declaration.clone(), true)
            .unwrap_err()
            .code,
        "declaration_order"
    );
}

#[test]
fn config_owner_refused_literal_is_absent_from_persisted_artifacts_and_views() {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"safe-reference"}}"#,
    )
    .unwrap();
    let owner = rig.owner("literal");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let literal = "never-persist-this-literal-secret";
    for value in [json!(literal), json!("{env:REFUSED_SECRET}")] {
        std::fs::write(
            rig.path.join("cortexkit/a.jsonc"),
            serde_json::to_vec(&json!({"credential":value})).unwrap(),
        )
        .unwrap();
        let error = owner
            .prepare(PlanKind::Change)
            .err()
            .expect("refused input");
        assert_eq!(error.code, "value_invalid");
        assert_eq!(error.detail, "a: /credential");
    }
    let history = serde_json::to_string(&owner.history().unwrap()).unwrap();
    assert!(!history.contains(literal));
    assert!(!history.contains("REFUSED_SECRET"));
    for entry in std::fs::read_dir(rig.path.join("store")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            let bytes = std::fs::read(entry.path()).unwrap();
            for forbidden in [literal.as_bytes(), b"REFUSED_SECRET"] {
                assert!(!bytes
                    .windows(forbidden.len())
                    .any(|window| window == forbidden));
            }
        }
    }
    for activity in owner.take_activities() {
        assert!(!activity.detail.contains(literal));
        assert!(!activity.detail.contains("REFUSED_SECRET"));
    }
}

#[test]
fn config_owner_recaptures_file_identity_after_presence_before_committing() {
    let rig = Rig::new();
    let bytes = br#"{"credential":{"vault_ref":"safe-reference"}}"#;
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), bytes).unwrap();
    let owner = rig.owner("recapture");
    rig.declare(&owner, 10);
    let prepared = owner.prepare(PlanKind::Stamp).unwrap();
    let plan = owner.finish_plan(prepared, daemon_review).unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    std::fs::write(rig.path.join("replacement.jsonc"), bytes).unwrap();
    std::fs::rename(
        rig.path.join("replacement.jsonc"),
        rig.path.join("cortexkit/a.jsonc"),
    )
    .unwrap();
    let error = owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
            1,
        )
        .unwrap_err();
    assert_eq!(error.code, "plan_stale");
    assert_eq!(
        owner.lock().unwrap().store.status().state,
        StoreState::Unstamped
    );
}

#[test]
fn config_owner_status_observes_changed_snapshot_without_committing_it() {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"safe-reference"},"limit":1}"#,
    )
    .unwrap();
    let owner = rig.owner("proposal");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"safe-reference"},"limit":2}"#,
    )
    .unwrap();
    assert_eq!(owner.status().unwrap().pending_proposals, 1);
    let view = owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap()
        .pending
        .unwrap();
    assert_eq!(view.effective["limit"], 1);
    assert!(owner
        .take_activities()
        .iter()
        .any(|activity| activity.event == "config.proposal"));
}

#[test]
fn config_owner_unrelated_invalid_layer_does_not_stale_a_reviewed_module() {
    let rig = Rig::new();
    let layer = |limit| {
        serde_json::to_vec(&json!({"credential":{"vault_ref":"safe-reference"},"limit":limit}))
            .unwrap()
    };
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), layer(1)).unwrap();
    std::fs::write(rig.path.join("cortexkit/b.jsonc"), layer(1)).unwrap();
    std::fs::write(rig.path.join("subc.jsonc"), serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":rig.path.join("program"),"registration_channel":true},"b":{"program":rig.path.join("program"),"registration_channel":true}}})).unwrap()).unwrap();
    let owner = rig.owner("unrelated");
    rig.declare(&owner, 10);
    owner
        .declare(
            ConnectionId::new(11),
            "b",
            ConfigDeclareRequest::new(
                rig.declaration.schema.clone(),
                rig.path.join("cortexkit/b.jsonc").to_string_lossy(),
                vec![],
            ),
            true,
        )
        .unwrap();
    rig.stamp(&owner);

    std::fs::write(rig.path.join("cortexkit/a.jsonc"), layer(2)).unwrap();
    let prepared = owner.prepare(PlanKind::Change).unwrap();
    assert_eq!(prepared.review_modules, BTreeSet::from(["a".into()]));
    std::fs::write(
        rig.path.join("cortexkit/b.jsonc"),
        br#"{"credential":"unrelated-refused-literal"}"#,
    )
    .unwrap();
    let plan = owner.finish_plan(prepared, daemon_review).unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    assert_eq!(
        owner
            .finish_approval(
                challenge.attempt(),
                &Rig::facts(),
                ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
                2
            )
            .unwrap(),
        2
    );
    let a = owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap()
        .pending
        .unwrap();
    let b = owner
        .module_get(ConnectionId::new(11), "b", ConfigGetRequest::new())
        .unwrap()
        .pending
        .unwrap();
    assert_eq!((a.generation, a.effective["limit"].clone()), (2, json!(2)));
    assert_eq!((b.generation, b.effective["limit"].clone()), (1, json!(1)));
    assert!(!serde_json::to_string(&owner.history().unwrap())
        .unwrap()
        .contains("unrelated-refused-literal"));
}

#[test]
fn config_owner_failed_fresh_capture_consumes_the_approval_attempt() {
    let rig = Rig::new();
    let valid = br#"{"credential":{"vault_ref":"safe-reference"}}"#;
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), valid).unwrap();
    let owner = rig.owner("failed-capture");
    rig.declare(&owner, 10);
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":"refused-literal"}"#,
    )
    .unwrap();
    assert_eq!(
        owner
            .finish_approval(
                challenge.attempt(),
                &Rig::facts(),
                ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
                1
            )
            .unwrap_err()
            .code,
        "plan_stale"
    );
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), valid).unwrap();
    assert_eq!(
        owner
            .finish_approval(
                challenge.attempt(),
                &Rig::facts(),
                ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
                1
            )
            .unwrap_err()
            .code,
        "approval_surface_unavailable"
    );
}

#[test]
fn config_owner_recovery_refuses_changes_but_can_plan_and_commit_restamp() {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"safe-reference"}}"#,
    )
    .unwrap();
    let owner = rig.owner("before-recovery");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    drop(owner);
    std::fs::write(
        rig.path.join("store/approved.json"),
        br#"{"format_version":999}"#,
    )
    .unwrap();
    let owner = rig.owner("recovery");
    assert_eq!(
        owner
            .declare(ConnectionId::new(20), "a", rig.declaration.clone(), true)
            .unwrap_err()
            .code,
        "config_gated"
    );
    assert_eq!(owner.status().unwrap().state, "store_unverified");
    assert_eq!(
        owner.prepare(PlanKind::Change).err().unwrap().code,
        "store_unverified"
    );
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::PresenceDecision::InteractiveApproval),
            1,
        )
        .unwrap();
    assert!(owner.is_recovering());
    let status = owner.status().unwrap();
    assert_eq!(status.state, "stamped");
    assert!(status
        .warnings
        .iter()
        .any(|warning| warning == "approved recovery exit is waiting for reconciliation"));
    owner.complete_authority_exit_for_test(1).unwrap();
    assert_eq!(owner.status().unwrap().state, "stamped");
}

#[test]
fn config_owner_idle_expiry_frees_plans_and_names_recent_expirations() {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"safe-reference"}}"#,
    )
    .unwrap();
    let owner = rig.owner("expiry");
    rig.declare(&owner, 10);
    let view = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    {
        let mut state = owner.inner.lock().unwrap();
        let deadline = state.plan_deadlines[&view.digest];
        state.expire_idle(deadline - std::time::Duration::from_nanos(1));
        assert!(state.gate.plan_modules(&view.digest).is_ok());
        state.expire_idle(deadline);
        assert!(state.plan_deadlines.is_empty());
        assert!(state.retained_plan_files.is_empty());
        assert!(state.gate.plan_modules(&view.digest).is_err());
    }
    assert_eq!(
        owner
            .begin_approval(&view.digest, &Rig::facts())
            .unwrap_err()
            .code,
        "plan_expired"
    );
    let replacement = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    assert_eq!(replacement.digest, view.digest);
    owner
        .desktop_view_delivered(ConnectionId::new(7), &replacement)
        .unwrap();
    assert!(owner
        .begin_approval(&replacement.digest, &Rig::facts())
        .is_ok());
}

#[test]
fn config_owner_retains_original_file_objects_while_plan_is_pending() {
    let rig = Rig::new();
    let layer = rig.path.join("cortexkit/a.jsonc");
    std::fs::write(&layer, br#"{"credential":{"vault_ref":"safe-reference"}}"#).unwrap();
    let owner = rig.owner("retained-files");
    rig.declare(&owner, 10);
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    let before = subc_os::open_regular_nofollow(&layer).unwrap();
    let identity = subc_os::retained_file_identity(&before).unwrap();
    drop(before);
    let retired = rig.path.join("retired-layer");
    std::fs::rename(&layer, &retired).unwrap();
    std::fs::remove_file(retired).unwrap();
    {
        let state = owner.inner.lock().unwrap();
        assert!(state.retained_plan_files[&plan.digest]
            .iter()
            .any(|file| subc_os::retained_file_identity(file).unwrap() == identity));
    }
    std::fs::write(&layer, br#"{"credential":{"vault_ref":"safe-reference"}}"#).unwrap();
    let (identity_before, identity_now) = (
        identity,
        subc_os::retained_file_identity(&subc_os::open_regular_nofollow(&layer).unwrap()).unwrap(),
    );
    assert_ne!(identity_before, identity_now);
    assert_eq!(
        owner
            .begin_approval(&plan.digest, &Rig::facts())
            .unwrap_err()
            .code,
        "plan_stale"
    );
}

#[test]
fn config_owner_declaration_order_precedes_project_registry_refusal() {
    let rig = Rig::new();
    let owner = rig.owner("project-order");
    let mut request = ConfigGetRequest::new();
    request.project_id = Some("unapproved-project".into());
    assert_eq!(
        owner
            .module_get(ConnectionId::new(10), "a", request)
            .unwrap_err()
            .code,
        "declaration_order"
    );
}

#[test]
fn config_owner_open_approval_prevents_expiry_and_failed_reply_keeps_plan_for_an_idle_hour() {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"safe-reference"}}"#,
    )
    .unwrap();
    let owner = rig.owner("open-expiry");
    rig.declare(&owner, 10);
    let view = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &view)
        .unwrap();
    let challenge = owner.begin_approval(&view.digest, &Rig::facts()).unwrap();
    {
        let mut state = owner.inner.lock().unwrap();
        let beyond = state.plan_deadlines[&view.digest] + PLAN_IDLE_TIMEOUT;
        state.expire_idle(beyond);
        assert!(state.gate.plan_modules(&view.digest).is_ok());
    }
    assert!(owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::PresenceDecision::Declined),
            1
        )
        .is_err());
    let mut state = owner.inner.lock().unwrap();
    assert!(!state.gate.has_open_approval(&view.digest));
    let deadline = state.plan_deadlines[&view.digest];
    state.expire_idle(deadline - std::time::Duration::from_nanos(1));
    assert!(state.gate.plan_modules(&view.digest).is_ok());
    state.expire_idle(deadline);
    assert!(state.gate.plan_modules(&view.digest).is_err());
}

#[test]
fn config_phone_disconnect_refreshes_the_preserved_plans_idle_deadline() {
    let rig = Rig::new();
    let path = rig.path.join("cortexkit/a.jsonc");
    std::fs::write(
        &path,
        br#"{"credential":{"vault_ref":"safe-reference"},"limit":1}"#,
    )
    .unwrap();
    let owner = rig.owner("phone-expiry");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    std::fs::write(
        &path,
        br#"{"credential":{"vault_ref":"safe-reference"},"limit":2}"#,
    )
    .unwrap();
    let view = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    let mut facts = Rig::facts();
    facts.desktop_app = false;
    facts.attested_operator_app = false;
    facts.os_prompt_available = false;
    facts.phone = Some(super::super::PhoneSurface {
        module_id: "callosum".into(),
        connection: 90,
        spawn_generation: 1,
        enabled_by_policy: true,
        opted_in: true,
        approved_declaration: true,
        reserved: true,
        channel_registered: true,
        program_admitted: true,
        active_registration: true,
    });
    let challenge = owner.begin_approval(&view.digest, &facts).unwrap();
    let mut invalidated = owner
        .bind_phone_rpc(challenge.attempt(), ConnectionId::new(90), 12)
        .unwrap();
    let old_deadline = Instant::now() - Duration::from_secs(1);
    owner
        .inner
        .lock()
        .unwrap()
        .plan_deadlines
        .insert(view.digest.clone(), old_deadline);
    owner.disconnected(ConnectionId::new(90));
    assert_eq!(
        invalidated.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    );
    let mut state = owner.inner.lock().unwrap();
    assert!(!state.gate.has_open_approval(&view.digest));
    state.expire_idle(old_deadline);
    assert!(
        state.gate.plan_modules(&view.digest).is_ok(),
        "phone disconnect must start a new idle hour, not expire the just-withdrawn plan"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn config_admission_windows_protocol_none_keeps_binding_until_child_exit() {
    use crate::supervise::{RestartPolicy, Supervisor};
    use std::sync::Arc;
    let rig = Rig::new();
    let program = rig.path.join("program.exe");
    std::fs::copy(
        std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/ping.exe"),
        &program,
    )
    .unwrap();
    let covered = rig.path.join("covered.txt");
    std::fs::write(&covered, b"approved").unwrap();
    std::fs::write(rig.path.join("subc.jsonc"),serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":program,"args":["-t","127.0.0.1"],"protocol":"none","program_files":[covered]}}})).unwrap()).unwrap();
    let owner = Arc::new(rig.owner("windows-no-hello"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner);
    let spec = crate::daemon_config::load(rig.path.join("subc.jsonc"))
        .unwrap()
        .unwrap()
        .modules[0]
        .module_spec();
    let module = supervisor.spawn_async(spec).await.unwrap();
    let alive = module.status().unwrap().process_alive;
    let excluded = std::fs::OpenOptions::new()
        .write(true)
        .open(&covered)
        .err()
        .and_then(|error| error.raw_os_error());
    module.stop().await.unwrap();
    let released = std::fs::OpenOptions::new()
        .write(true)
        .open(&covered)
        .is_ok();
    assert!(alive);
    assert_eq!(
        excluded,
        Some(32),
        "a child without HELLO retains its Windows content witness until exit"
    );
    assert!(
        released,
        "a reaped child cannot keep a write-exclusion handle alive"
    );
}

#[cfg(windows)]
async fn windows_removed_registration_preserves_binding(candidate: bool) {
    use crate::supervise::{RestartPolicy, Supervisor, SupervisorHandle};
    use std::sync::Arc;
    let rig = Rig::new();
    let program = rig.path.join("program.exe");
    std::fs::copy(
        std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/ping.exe"),
        &program,
    )
    .unwrap();
    let covered = rig.path.join("covered.txt");
    std::fs::write(&covered, b"approved").unwrap();
    std::fs::write(rig.path.join("subc.jsonc"),serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":program,"args":["-t","127.0.0.1"],"protocol":"subc","registration_channel":true,"overlap":"safe","program_files":[covered]}}})).unwrap()).unwrap();
    let owner = Arc::new(rig.owner("removed-registration"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let handle = SupervisorHandle::new();
    handle.capture_registration_channels_for_test();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone())
        .with_forwarding(forwarding.clone())
        .with_drain_timeout(std::time::Duration::from_millis(20));
    let spec = crate::daemon_config::load(rig.path.join("subc.jsonc"))
        .unwrap()
        .unwrap()
        .modules[0]
        .module_spec();
    let module = supervisor.spawn_async(spec).await.unwrap();
    let initial_proof = handle.registration_proof_for_test("a").unwrap();
    handle
        .registration_for_test("a")
        .unwrap()
        .wait_launch()
        .await;
    assert!(
        handle
            .wait_program_binding_for_test("a", &initial_proof)
            .await
    );
    assert!(initial_proof.nonce().is_none());
    let handler = crate::ControlHandler::with_forwarding(registry.clone(), forwarding)
        .with_supervisor(handle.clone())
        .with_config_runtime(owner);
    let mut manifest = subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build();
    manifest.ready = Some(true);
    let hello = subc_protocol::ModuleHelloBody {
        manifest,
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: initial_proof.nonce(),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    let (incumbent_sender, mut incumbent_frames) = tokio::sync::mpsc::channel(8);
    let (proof, swapping) = if candidate {
        let incumbent = ConnectionId::new(770);
        initial_proof.bind(incumbent);
        let reply = handler
            .handle_control_frame(
                &crate::RouteCtx {
                    connection_id: incumbent,
                    egress: crate::FrameSink::new(incumbent_sender),
                },
                frame.clone(),
            )
            .await
            .unwrap();
        assert!(reply.is_empty());
        assert_eq!(
            incumbent_frames.try_recv().unwrap().frame.header.ty,
            subc_protocol::FrameType::HelloAck
        );
        let swapping = {
            let module = module.clone();
            tokio::spawn(async move { module.swap(Some(std::time::Duration::from_secs(5))).await })
        };
        let proof = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(proof) = handle.candidate_registration_proof_for_test("a") {
                    if handle.has_live_verified_program_binding_for_test("a", &proof)
                        && handle.wait_program_binding_for_test("a", &proof).await
                    {
                        break proof;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        (proof, Some(swapping))
    } else {
        (initial_proof, None)
    };
    let launch = handle.registration_for_test("a").unwrap();
    launch.wait_launch().await;
    assert_eq!(launch.is_candidate(), candidate);
    let hold = Arc::new(std::sync::Barrier::new(2));
    let handler = handler.with_hello_registry_to_forwarding_hold(hold.clone());
    let connection = ConnectionId::new(771);
    proof.bind(connection);
    let (sender, mut frames) = tokio::sync::mpsc::channel(8);
    let ctx = crate::RouteCtx {
        connection_id: connection,
        egress: crate::FrameSink::new(sender),
    };
    let runtime = tokio::runtime::Handle::current();
    let pending = {
        let handler = handler.clone();
        std::thread::spawn(move || {
            runtime
                .block_on(handler.handle_control_frame(&ctx, frame))
                .unwrap()
        })
    };
    hold.wait();
    assert_eq!(registry.deregister_connection(connection).unwrap().len(), 1);
    hold.wait();
    let reply = pending.join().unwrap();
    let ack = frames.try_recv().unwrap().frame.header.ty;
    let retained = std::fs::OpenOptions::new()
        .write(true)
        .open(&covered)
        .err()
        .and_then(|error| error.raw_os_error());
    let launch_check = launch.check();
    // Inspect failed publication while the swap is still open and the child
    // is running; stopping the module would also cancel a successful swap.
    assert!(module.status().unwrap().process_alive);
    assert_eq!(
        launch_check,
        Err(crate::supervise::REGISTRATION_CHANNEL_CLOSED),
        "the removed publication must retire the candidate's channel authority before cleanup"
    );
    if candidate {
        assert!(registry
            .registration(crate::registry::RegistrationSlot::Candidate("a"))
            .unwrap()
            .is_none());
        assert!(
            handle
                .with_swap_candidate_admission("a", || ((), false))
                .is_some(),
            "an unpublished candidate must not consume the open swap's admission slot"
        );
    }
    handler.cleanup_connection(connection).unwrap();
    handler.cleanup_connection(ConnectionId::new(770)).unwrap();
    module.stop().await.unwrap();
    if let Some(swapping) = swapping {
        let _ = swapping.await.unwrap();
    }
    assert!(reply.is_empty());
    assert_eq!(ack, subc_protocol::FrameType::HelloAck);
    assert_eq!(
        retained,
        Some(32),
        "removed registration must not release retained content handles"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn config_admission_windows_removed_ordinary_publication_keeps_handles() {
    windows_removed_registration_preserves_binding(false).await;
}

#[cfg(windows)]
#[tokio::test]
async fn config_admission_windows_removed_candidate_publication_keeps_handles() {
    windows_removed_registration_preserves_binding(true).await;
}

async fn registration_declaration_poll(
    handler: std::sync::Arc<crate::ControlHandler>,
    op: &str,
) -> serde_json::Value {
    #[cfg(unix)]
    let user = subc_os::local_peer::PeerUser::Uid(subc_os::effective_uid());
    #[cfg(windows)]
    let user =
        subc_os::local_peer::PeerUser::Sid(subc_os::named_pipe::current_user_sid_string().unwrap());
    handler.record_observed_peer(
        ConnectionId::new(50),
        subc_os::local_peer::ObservedPeer::Observed(subc_os::local_peer::PeerObservation {
            user,
            pid: std::process::id(),
            exe: None,
            signing_id: None,
        }),
    );
    let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
    let ctx = crate::RouteCtx {
        connection_id: ConnectionId::new(50),
        egress: crate::FrameSink::new(sender),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Request,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&json!({"op": op})).unwrap(),
    )
    .unwrap();
    let mut frames = handler.handle_control_frame(&ctx, frame).await.unwrap();
    let frame = if frames.is_empty() {
        receiver.recv().await.unwrap().frame.clone()
    } else {
        frames.remove(0)
    };
    serde_json::from_slice(&frame.body).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_declaration_polled_reads_do_not_join_the_owner() {
    let rig = Rig::new();
    let owner = std::sync::Arc::new(rig.owner("polled-held-owner"));
    rig.stamp(&owner);
    owner.complete_authority_exit_for_test(1).unwrap();
    let handler = std::sync::Arc::new(
        crate::ControlHandler::new(std::sync::Arc::new(crate::Registry::default()))
            .with_config_runtime(owner.clone()),
    );
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let held_owner = owner.clone();
    let holder = std::thread::spawn(move || {
        let _guard = held_owner.lock().unwrap();
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    entered_rx.await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        for op in ["config.status", "catalog.list", "supervisor.list"] {
            let body = registration_declaration_poll(handler.clone(), op).await;
            assert!(body.get("code").is_none(), "{op}: {body}");
        }
    })
    .await;
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    result.expect("polls must complete before releasing the owner");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_declaration_raw_config_edit_is_pending_while_owner_is_held() {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"approved"},"limit":2}"#,
    )
    .unwrap();
    let owner = std::sync::Arc::new(rig.owner("polled-config-edit"));
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    owner.complete_authority_exit_for_test(1).unwrap();
    assert_eq!(owner.polled_status().unwrap().pending_proposals, 0);
    let handler = std::sync::Arc::new(
        crate::ControlHandler::new(std::sync::Arc::new(crate::Registry::default()))
            .with_config_runtime(owner.clone()),
    );
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let held_owner = owner.clone();
    let holder = std::thread::spawn(move || {
        let _guard = held_owner.lock().unwrap();
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    entered_rx.await.unwrap();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"approved"},"limit":3}"#,
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        registration_declaration_poll(handler, "config.status"),
    )
    .await;
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let body = result.expect("raw file comparison must not acquire the held owner");
    assert_eq!(body["status"]["pending_proposals"], 1, "{body}");
}

#[test]
fn registration_declaration_commit_is_visible_in_the_next_projection() {
    let rig = Rig::new();
    let owner = rig.owner("polled-commit");
    let before = owner.polled_status().unwrap();
    assert!(before.approval_channels.is_empty());
    rig.stamp(&owner);
    let committed = owner.polled_status().unwrap();
    assert_eq!(committed.approval_channels.len(), 1);
    assert!(committed.revision > before.revision);
    assert!(committed
        .warnings
        .iter()
        .any(|warning| warning.contains("registration channel cutover")));
    owner.complete_authority_exit_for_test(1).unwrap();
    assert_eq!(owner.polled_status().unwrap().state, "stamped");
}

#[cfg(unix)]
#[tokio::test]
async fn registration_declaration_stamp_names_all_enabled_entries_without_hello() {
    use crate::supervise::{ModuleState, RestartPolicy, Supervisor};
    let rig = Rig::new();
    let path = rig.path.join("subc.jsonc");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"version":1,"modules":{
            "stopped":{"program":"/bin/sleep","args":["600"]},
            "crashed":{"program":"/bin/sleep","args":["not-a-number"],"registration_channel":false},
            "never-registered":{"program":"/bin/sleep","args":["600"]},
            "disabled":{"program":"/bin/sleep","enabled":false},
            "non-wire":{"program":"/bin/sleep","protocol":"none"},
            "supported":{"program":"/bin/sleep","registration_channel":true}
        }}))
        .unwrap(),
    )
    .unwrap();
    let registry = std::sync::Arc::new(crate::Registry::default());
    let policy = RestartPolicy::new(0, Duration::from_millis(1));
    let supervisor = Supervisor::new_for_test(registry.clone(), policy)
        .with_drain_timeout(Duration::from_millis(10));
    let (config, _) =
        crate::daemon_config::parse_with_activity(&std::fs::read_to_string(&path).unwrap(), &path)
            .unwrap();
    let mut actors = Vec::new();
    for id in ["stopped", "crashed", "never-registered"] {
        let entry = config
            .modules
            .iter()
            .find(|entry| entry.module_id == id)
            .unwrap();
        actors.push(
            supervisor
                .supervise_configured_requested_async(
                    entry.module_spec(),
                    true,
                    Default::default(),
                    None,
                    policy,
                    Duration::from_secs(120),
                    None,
                )
                .await
                .unwrap(),
        );
    }
    actors[0].stop().await.unwrap();
    assert_eq!(actors[0].status().unwrap().state, ModuleState::Stopped);
    tokio::time::timeout(Duration::from_secs(5), async {
        while actors[1].status().unwrap().state != ModuleState::Failed {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("invalid sleep argument must produce a real failed spawn");
    assert!(!actors[1].status().unwrap().process_alive);
    assert!(actors[2].status().unwrap().process_alive);
    assert!(!actors[2].status().unwrap().registration_active);
    assert!(registry.list_modules().unwrap().1.is_empty());
    let owner = rig.owner("stamp-census");
    let error = owner
        .prepare(PlanKind::Stamp)
        .err()
        .expect("missing declarations must refuse a stamp");
    assert_eq!(error.code, subc_control::REGISTRATION_CHANNEL_UNSUPPORTED);
    assert!(
        error.detail.contains("stopped")
            && error.detail.contains("crashed")
            && error.detail.contains("never-registered"),
        "{}",
        error.detail
    );
    assert!(
        !error.detail.contains("disabled")
            && !error.detail.contains("non-wire")
            && !error.detail.contains("supported,")
    );
    assert!(owner.polled_status().unwrap().approval_channels.is_empty());
    for actor in actors {
        actor.stop().await.unwrap();
    }
}

#[test]
fn registration_declaration_polled_status_preserves_gated_declaration_diagnostics() {
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"approved"},"limit":2}"#,
    )
    .unwrap();
    let owner = rig.owner("polled-declaration-diagnostics");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    owner.complete_authority_exit_for_test(1).unwrap();
    let mut changed = rig.declaration.clone();
    changed.schema["properties"]["limit"]["default"] = json!(7);
    owner
        .declare(ConnectionId::new(11), "a", changed, true)
        .unwrap();
    assert!(owner
        .polled_status()
        .unwrap()
        .gated_modules
        .iter()
        .any(|module| module == "a"));
}
