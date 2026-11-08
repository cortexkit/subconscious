use super::*;
use crate::caller_identity::CallerIdentity;
use crate::config_store::StoreRoot;
use serde_json::json;
use subc_protocol::config::ConfigGetRequest;

struct Rig {
    path: std::path::PathBuf,
    declaration: ConfigDeclareRequest,
}
impl Rig {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
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
