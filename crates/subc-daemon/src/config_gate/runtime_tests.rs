use super::*;
use crate::caller_identity::CallerIdentity;
use crate::config_store::StoreRoot;
use serde_json::json;
use subc_protocol::config::ConfigGetRequest;

struct Rig {
    path: std::path::PathBuf,
    declaration: ConfigDeclareRequest,
}

#[cfg(unix)]
#[path = "admission_tests.rs"]
mod admission_tests;
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
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(path.join("cortexkit")).unwrap();
        std::fs::create_dir(path.join("store")).unwrap();
        std::fs::write(path.join("program"), b"captured program identity").unwrap();
        std::fs::write(
            path.join("subc.jsonc"),
            serde_json::to_vec(
                &json!({"version":1,"modules":{"a":{"program":path.join("program")}}}),
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
        Self { path, declaration }
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

    fn stamp(&self, owner: &RuntimeOwner) -> u64 {
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
    std::fs::write(rig.path.join("subc.jsonc"), serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":rig.path.join("program")},"b":{"program":rig.path.join("program")}}})).unwrap()).unwrap();
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
    rig.declare(&owner, 20);
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
        nonce_verified: true,
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
