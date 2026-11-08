use super::*;
use crate::caller_identity::CallerIdentity;
use crate::config_store::{StoreRoot, StoreState};
use serde_json::{json, Value};
use subc_config::{ApplyKind, FieldChange, InputState, RecordedValue};
use subc_protocol::consent::{ConsentDecision, ConsentResponse};

fn captured(value: Value) -> CapturedInput {
    CapturedInput {
        state: InputState::Present {
            digest: subc_config::raw_digest(&serde_json::to_vec(&value).unwrap()),
        },
        value: RecordedValue::new(value),
    }
}
fn layer(module: &str, path: &str) -> InputKey {
    InputKey::Layer {
        module: module.into(),
        path: path.into(),
    }
}
fn program(module: &str) -> InputKey {
    InputKey::Program {
        module: module.into(),
        path: "/covered/program".into(),
    }
}
fn snapshot() -> GateSnapshot {
    let mut inputs = BTreeMap::from([(InputKey::Daemon, captured(json!({"modules":{}})))]);
    for module in ["a", "b", "callosum"] {
        inputs.insert(
            InputKey::Entry(module.into()),
            captured(json!({"reserved":true})),
        );
        inputs.insert(layer(module, "user"), captured(json!(10)));
        inputs.insert(layer(module, "project"), captured(json!(15)));
        inputs.insert(
            InputKey::Declaration(module.into()),
            captured(json!({"schema":"approved"})),
        );
        inputs.insert(program(module), captured(json!("program-v1")));
    }
    GateSnapshot { inputs }
}
fn phone() -> PhoneSurface {
    PhoneSurface {
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
    }
}
fn facts(desktop: bool) -> ApprovalFacts {
    ApprovalFacts {
        connection: 7,
        owner_uid: true,
        desktop_app: desktop,
        attested_operator_app: desktop,
        os_prompt_available: true,
        phone: Some(phone()),
        caller: CallerIdentity::default(),
    }
}
fn complex(_: &GateSnapshot, _: &GateSnapshot) -> Result<Changeset> {
    let mut change = Changeset::new();
    change.apply_plan = json!({"effect":"full review required"});
    Ok(change)
}
fn scalar_review(old: &GateSnapshot, new: &GateSnapshot) -> Result<Changeset> {
    let mut change = Changeset::new();
    change.fields.push(FieldChange::new(
        "a",
        "/v",
        old.inputs[&layer("a", "user")].value.clone(),
        new.inputs[&layer("a", "user")].value.clone(),
        ApplyKind::Live,
    ));
    Ok(change)
}

struct Rig {
    path: std::path::PathBuf,
    store: ConfigStore,
    gate: ConfigGate,
    current: GateSnapshot,
}
impl Rig {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "subc-config-gate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        let store = ConfigStore::open_digest_only_for_tests(
            StoreRoot::from_service_definition(path.clone()).unwrap(),
        );
        Self {
            path,
            store,
            gate: ConfigGate::new("isolated-daemon".into()),
            current: snapshot(),
        }
    }
    fn approve(&mut self, view: &ConfigPlanView, facts: &ApprovalFacts) -> Result<u64> {
        let challenge =
            self.gate
                .begin_approval(&view.digest, &mut self.store, &self.current, facts)?;
        let reply = match &challenge {
            ApprovalChallenge::Phone {
                connection,
                correlation,
                request,
                ..
            } => ApprovalReply::Phone {
                connection: *connection,
                correlation: *correlation,
                response: Some(ConsentResponse::new(request, ConsentDecision::Approved)),
            },
            _ => ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
        };
        self.gate.finish_approval(
            challenge.attempt(),
            reply,
            &mut self.store,
            &self.current,
            facts,
            1,
        )
    }
    fn stamp(&mut self) -> u64 {
        let view = self
            .gate
            .plan(
                &mut self.store,
                PlanKind::Stamp,
                self.current.clone(),
                &self.current,
                complex,
            )
            .unwrap();
        self.approve(&view, &facts(true)).unwrap()
    }
    fn edit(&mut self, key: InputKey, value: Value) {
        self.current.inputs.insert(key, captured(value));
    }
    fn change(&mut self, small: bool) -> ConfigPlanView {
        self.gate
            .plan(
                &mut self.store,
                PlanKind::Change,
                self.current.clone(),
                &self.current,
                if small { scalar_review } else { complex },
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
fn approval_stamp_is_desktop_only_and_full_view_is_digest_bound() {
    let mut rig = Rig::new();
    let view = rig
        .gate
        .plan(
            &mut rig.store,
            PlanKind::Stamp,
            rig.current.clone(),
            &rig.current,
            complex,
        )
        .unwrap();
    assert_eq!(view.digest, view.plan.digest().unwrap());
    assert!(serde_json::to_string(&view).unwrap().contains("program-v1"));
    assert!(serde_json::to_string(&view).unwrap().contains("approved"));
    let wrong = subc_config::raw_digest(b"a different digest");
    assert_eq!(
        rig.gate
            .begin_approval(&wrong, &mut rig.store, &rig.current, &facts(true))
            .unwrap_err()
            .code,
        "plan_unknown"
    );
    assert_eq!(
        rig.approve(&view, &facts(false)).unwrap_err().code,
        "approval_surface_unavailable"
    );
    assert_eq!(rig.store.status().state, StoreState::Unstamped);
    assert_eq!(rig.approve(&view, &facts(true)).unwrap(), 1);
    assert_eq!(
        rig.store.body().unwrap().generations[&1]
            .generation
            .plan_digest,
        view.digest
    );
    assert_eq!(
        rig.approve(&view, &facts(true)).unwrap_err().code,
        "plan_unknown"
    );
}

#[test]
fn approval_dev_desktop_feature_is_required_and_owner_uid_still_required() {
    let mut rig = Rig::new();
    let view = rig
        .gate
        .plan(
            &mut rig.store,
            PlanKind::Stamp,
            rig.current.clone(),
            &rig.current,
            complex,
        )
        .unwrap();
    let mut dev = facts(true);
    dev.attested_operator_app = false;
    if cfg!(feature = "dev-desktop-approval") {
        assert_eq!(rig.approve(&view, &dev).unwrap(), 1);
        assert_eq!(rig.approve(&view, &dev).unwrap_err().code, "plan_unknown");
    } else {
        assert_eq!(
            rig.approve(&view, &dev).unwrap_err().code,
            "approval_surface_unavailable"
        );
    }
    let mut rig = Rig::new();
    let view = rig
        .gate
        .plan(
            &mut rig.store,
            PlanKind::Stamp,
            rig.current.clone(),
            &rig.current,
            complex,
        )
        .unwrap();
    dev.owner_uid = false;
    assert_eq!(
        rig.approve(&view, &dev).unwrap_err().code,
        "approval_surface_unavailable"
    );
}

#[test]
fn approval_callosum_changes_cannot_use_the_phone_or_substitute_program() {
    for key in [
        InputKey::Entry("callosum".into()),
        layer("callosum", "user"),
        InputKey::Declaration("callosum".into()),
        program("callosum"),
    ] {
        let mut rig = Rig::new();
        rig.stamp();
        rig.edit(key, json!("substitute"));
        let view = rig.change(false);
        assert_eq!(
            rig.approve(&view, &facts(false)).unwrap_err().code,
            "approval_surface_unavailable"
        );
        assert_eq!(rig.approve(&view, &facts(true)).unwrap(), 2);
    }
}

#[test]
fn approval_full_daemon_document_cannot_hide_a_callosum_entry_edit() {
    let mut rig = Rig::new();
    rig.stamp();
    rig.edit(
        InputKey::Daemon,
        json!({"modules":{"callosum":{"program":"substitute"}}}),
    );
    let view = rig.change(false);
    assert_eq!(
        rig.approve(&view, &facts(false)).unwrap_err().code,
        "approval_surface_unavailable"
    );
    assert_eq!(rig.approve(&view, &facts(true)).unwrap(), 2);
    rig.edit(
        InputKey::Daemon,
        json!({"modules":{"callosum":{"program":"substitute"}},"restart":{"limit":3}}),
    );
    let view = rig.change(false);
    assert_eq!(rig.approve(&view, &facts(false)).unwrap(), 3);
}

#[test]
fn approval_callosum_launch_manifest_needs_desktop_even_without_a_scalar_diff() {
    let mut rig = Rig::new();
    rig.stamp();
    let view = rig
        .gate
        .plan(
            &mut rig.store,
            PlanKind::Change,
            rig.current.clone(),
            &rig.current,
            |old, new| {
                let mut change = complex(old, new)?;
                change
                    .launch_manifests
                    .insert("callosum".into(), json!({"program":"replacement"}));
                Ok(change)
            },
        )
        .unwrap();
    assert_eq!(
        rig.approve(&view, &facts(false)).unwrap_err().code,
        "approval_surface_unavailable"
    );
    assert_eq!(rig.approve(&view, &facts(true)).unwrap(), 2);
}

#[test]
fn approval_phone_requires_every_live_registration_proof() {
    for field in 0..8 {
        let mut rig = Rig::new();
        rig.stamp();
        rig.edit(layer("a", "user"), json!(20));
        let view = rig.change(false);
        let mut caller = facts(false);
        let phone = caller.phone.as_mut().unwrap();
        match field {
            0 => phone.enabled_by_policy = false,
            1 => phone.opted_in = false,
            2 => phone.approved_declaration = false,
            3 => phone.reserved = false,
            4 => phone.nonce_verified = false,
            5 => phone.program_admitted = false,
            6 => phone.active_registration = false,
            _ => caller.phone = None,
        }
        assert_eq!(
            rig.approve(&view, &caller).unwrap_err().code,
            "approval_surface_unavailable",
            "proof {field}"
        );
        assert_eq!(rig.store.body().unwrap().last_generation, 1);
    }
}

#[test]
fn approval_phone_reply_binds_connection_request_digest_vector_and_revision() {
    for fault in 0..8 {
        let mut rig = Rig::new();
        rig.stamp();
        rig.edit(layer("a", "user"), json!(20));
        let view = rig.change(false);
        let mut caller = facts(false);
        let challenge = rig
            .gate
            .begin_approval(&view.digest, &mut rig.store, &rig.current, &caller)
            .unwrap();
        let ApprovalChallenge::Phone {
            attempt,
            mut connection,
            mut correlation,
            request,
        } = challenge
        else {
            panic!("phone card required")
        };
        assert!(request.is_consistent());
        let mut response = ConsentResponse::new(&request, ConsentDecision::Approved);
        match fault {
            0 => connection += 1,
            1 => correlation += 1,
            2 => response.plan_digest = subc_config::raw_digest(b"wrong"),
            3 => response.base_revision += 1,
            4 => response.input_vector.inputs.clear(),
            5 => response.decision = ConsentDecision::Declined,
            6 => caller.phone.as_mut().unwrap().spawn_generation += 1,
            _ => caller.phone = None,
        }
        assert!(rig
            .gate
            .finish_approval(
                attempt,
                ApprovalReply::Phone {
                    connection,
                    correlation,
                    response: Some(response)
                },
                &mut rig.store,
                &rig.current,
                &caller,
                1
            )
            .is_err());
        assert_eq!(rig.store.body().unwrap().last_generation, 1);
        assert_eq!(rig.approve(&view, &facts(false)).unwrap(), 2);
    }
}

#[test]
fn approval_unanswered_declined_and_disconnected_leave_plan_pending() {
    let mut rig = Rig::new();
    rig.stamp();
    rig.edit(layer("a", "user"), json!(20));
    let view = rig.change(false);
    let caller = facts(false);
    let challenge = rig
        .gate
        .begin_approval(&view.digest, &mut rig.store, &rig.current, &caller)
        .unwrap();
    let ApprovalChallenge::Phone {
        attempt,
        connection,
        correlation,
        ..
    } = challenge
    else {
        panic!("phone")
    };
    assert_eq!(
        rig.gate
            .finish_approval(
                attempt,
                ApprovalReply::Phone {
                    connection,
                    correlation,
                    response: None
                },
                &mut rig.store,
                &rig.current,
                &caller,
                1
            )
            .unwrap_err()
            .code,
        "approval_declined"
    );
    let challenge = rig
        .gate
        .begin_approval(&view.digest, &mut rig.store, &rig.current, &caller)
        .unwrap();
    assert_eq!(
        rig.gate.invalidate_phone(connection),
        vec![challenge.attempt()]
    );
    assert_eq!(
        rig.gate
            .finish_approval(
                challenge.attempt(),
                ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
                &mut rig.store,
                &rig.current,
                &caller,
                1
            )
            .unwrap_err()
            .code,
        "approval_surface_unavailable"
    );
    assert_eq!(rig.approve(&view, &caller).unwrap(), 2);
}

#[test]
fn approval_read_set_changes_are_stale_and_never_return_a_fresh_digest() {
    let keys = [
        InputKey::Daemon,
        layer("a", "user"),
        layer("a", "project"),
        InputKey::Declaration("a".into()),
        program("a"),
        layer("a", "new-project"),
    ];
    for key in keys {
        let mut rig = Rig::new();
        rig.stamp();
        rig.edit(layer("a", "user"), json!(20));
        let view = rig.change(false);
        let challenge = rig
            .gate
            .begin_approval(&view.digest, &mut rig.store, &rig.current, &facts(true))
            .unwrap();
        rig.edit(key, json!("changed after review"));
        let error = rig
            .gate
            .finish_approval(
                challenge.attempt(),
                ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
                &mut rig.store,
                &rig.current,
                &facts(true),
                1,
            )
            .unwrap_err();
        assert_eq!(error.code, "plan_stale");
        assert!(!error.detail.contains("sha256:"));
        assert_eq!(rig.store.body().unwrap().last_generation, 1);
    }
}

#[test]
fn approval_status_ack_and_unrelated_proposal_or_declaration_do_not_stale_plan() {
    let mut rig = Rig::new();
    let stamp = rig.stamp();
    rig.edit(layer("a", "user"), json!(20));
    let view = rig.change(false);
    let base = view.base_revision;
    let before = rig.store.status().revision;
    rig.store
        .record_delivery(
            &Tuple {
                module: "b".into(),
                project: None,
            },
            stamp,
            true,
        )
        .unwrap();
    assert!(rig.store.status().revision > before);
    rig.edit(layer("b", "user"), json!(99));
    rig.edit(InputKey::Declaration("b".into()), json!({"schema":"new"}));
    rig.store.observe(rig.current.to_store().unwrap()).unwrap();
    assert_eq!(rig.store.status().base_revision, base);
    assert_eq!(rig.approve(&view, &facts(true)).unwrap(), 2);
    let proposal = rig.store.body().unwrap().proposal.as_ref().unwrap();
    assert!(proposal.diff.keys().any(|key| key.contains("b")));
    assert!(!proposal
        .diff
        .contains_key(&serde_json::to_string(&layer("a", "user")).unwrap()));
}

#[test]
fn approval_a_generation_commit_stales_the_old_base() {
    let mut rig = Rig::new();
    rig.stamp();
    rig.edit(layer("a", "user"), json!(20));
    let old = rig.change(false);
    rig.edit(layer("b", "user"), json!(30));
    let new = rig.change(false);
    rig.approve(&new, &facts(true)).unwrap();
    assert_eq!(
        rig.approve(&old, &facts(true)).unwrap_err().code,
        "plan_stale"
    );
}

#[test]
fn approval_os_prompt_exact_boundary_and_lossless_sanitizer() {
    let mut caller = facts(false);
    caller.phone = None;
    for length in [200, 201] {
        let mut rig = Rig::new();
        rig.stamp();
        rig.edit(layer("a", "user"), json!(""));
        let preliminary = rig.change(true);
        let challenge = rig
            .gate
            .begin_approval(&preliminary.digest, &mut rig.store, &rig.current, &caller)
            .unwrap();
        let base_len = challenge.prompt().unwrap().chars().count();
        rig.edit(layer("a", "user"), json!("x".repeat(length - base_len)));
        let view = rig.change(true);
        if length == 200 {
            let challenge = rig
                .gate
                .begin_approval(&view.digest, &mut rig.store, &rig.current, &caller)
                .unwrap();
            assert_eq!(challenge.prompt().unwrap().chars().count(), 200);
            assert_eq!(rig.approve(&view, &caller).unwrap(), 2);
        } else {
            assert_eq!(
                rig.approve(&view, &caller).unwrap_err().code,
                "approval_surface_unavailable"
            );
        }
    }
    for bad in ["invisible\u{202e}", "line\nbreak", "zero\u{200b}width"] {
        let mut rig = Rig::new();
        rig.stamp();
        rig.edit(layer("a", "user"), json!(bad));
        let view = rig.change(true);
        assert_eq!(
            rig.approve(&view, &caller).unwrap_err().code,
            "approval_surface_unavailable"
        );
    }
    let mut rig = Rig::new();
    rig.stamp();
    rig.edit(layer("a", "user"), json!(11));
    let view = rig
        .gate
        .plan(
            &mut rig.store,
            PlanKind::Change,
            rig.current.clone(),
            &rig.current,
            |old, new| {
                let mut change = scalar_review(old, new)?;
                change.fields[0].path = "x".repeat(65);
                Ok(change)
            },
        )
        .unwrap();
    assert_eq!(
        rig.approve(&view, &caller).unwrap_err().code,
        "approval_surface_unavailable"
    );
}

#[test]
fn approval_non_small_kinds_and_missing_os_prompt_never_fall_back_to_log() {
    let mut rig = Rig::new();
    rig.stamp();
    rig.edit(layer("a", "user"), json!(11));
    let mut caller = facts(false);
    caller.phone = None;
    let view = rig.change(false);
    assert_eq!(
        rig.approve(&view, &caller).unwrap_err().code,
        "approval_surface_unavailable"
    );
    let view = rig.change(true);
    caller.os_prompt_available = false;
    assert_eq!(
        rig.approve(&view, &caller).unwrap_err().code,
        "approval_surface_unavailable"
    );
    assert!(missing_surfaces(&caller, false)
        .iter()
        .any(|text| text.contains("polkit")));
    assert!(missing_surfaces(&caller, true)
        .iter()
        .any(|text| text.contains("recovery")));
    for kind in [PlanKind::Unstamp, PlanKind::Rollback(1)] {
        let view = if let PlanKind::Rollback(id) = kind {
            rig.gate.rollback(
                &mut rig.store,
                id,
                &rig.current,
                &rig.current,
                scalar_review,
            )
        } else {
            rig.gate.plan(
                &mut rig.store,
                kind,
                rig.current.clone(),
                &rig.current,
                scalar_review,
            )
        }
        .unwrap();
        caller.os_prompt_available = true;
        assert_eq!(
            rig.approve(&view, &caller).unwrap_err().code,
            "approval_surface_unavailable"
        );
    }
}

fn bound_review(_: &GateSnapshot, candidate: &GateSnapshot) -> Result<Changeset> {
    use subc_config::{Layer, LayerValue, Schema, ValidationContext};
    let schema=Schema::new(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"properties":{"n":{"type":"number","description":"A configurable value","default":5,"x-ck-apply":"live","x-ck-layers":["user","project"],"x-ck-bound":"raise_only"}}})).unwrap();
    let number = |path: &str| match &candidate.inputs[&layer("a", path)].value {
        RecordedValue::Value { value } => value.clone(),
        _ => panic!("number"),
    };
    schema
        .review_tuple(
            &[
                LayerValue::new(Layer::User, json!({"n":number("user")})),
                LayerValue::new(Layer::Project, json!({"n":number("project")})),
            ],
            &ValidationContext::new("a", Some("project-one".into())),
        )
        .map_err(|error| {
            GateError::new(
                serde_json::to_value(error.code).unwrap().as_str().unwrap(),
                format!("{} {}", error.path, error.project_id.unwrap()),
            )
        })?;
    complex(&GateSnapshot::default(), candidate)
}

#[test]
fn approval_raise_only_checks_unchanged_project_then_binds_it_in_read_set() {
    let mut rig = Rig::new();
    rig.stamp();
    assert!(bound_review(&rig.current, &rig.current).is_ok());
    rig.edit(layer("a", "user"), json!(20));
    let error = rig
        .gate
        .plan(
            &mut rig.store,
            PlanKind::Change,
            rig.current.clone(),
            &rig.current,
            bound_review,
        )
        .unwrap_err();
    assert_eq!(error.code, "bound_violated");
    assert!(error.detail.contains("project-one"));
    rig.edit(layer("a", "project"), json!(25));
    let view = rig
        .gate
        .plan(
            &mut rig.store,
            PlanKind::Change,
            rig.current.clone(),
            &rig.current,
            bound_review,
        )
        .unwrap();
    rig.edit(layer("a", "project"), json!(30));
    assert_eq!(
        rig.approve(&view, &facts(true)).unwrap_err().code,
        "plan_stale"
    );
}

#[test]
fn approval_rollback_reverses_config_but_shows_current_program_and_is_stale_checked() {
    let mut rig = Rig::new();
    rig.stamp();
    rig.edit(layer("a", "user"), json!(20));
    rig.edit(layer("a", "project"), json!(25));
    let view = rig.change(false);
    rig.approve(&view, &facts(true)).unwrap();
    rig.edit(program("a"), json!("program-v2-on-disk"));
    let view = rig
        .gate
        .rollback(&mut rig.store, 1, &rig.current, &rig.current, bound_review)
        .unwrap();
    let shown = serde_json::to_string(&view.plan.changeset.apply_plan["snapshot"]).unwrap();
    assert!(shown.contains("program-v2-on-disk"));
    assert_eq!(
        rig.gate.plans[&view.digest].snapshot.inputs[&program("a")].value,
        RecordedValue::new(json!("program-v2-on-disk"))
    );
    rig.edit(program("a"), json!("program-v3-in-place"));
    assert_eq!(
        rig.approve(&view, &facts(true)).unwrap_err().code,
        "plan_stale"
    );
    let view = rig
        .gate
        .rollback(&mut rig.store, 1, &rig.current, &rig.current, bound_review)
        .unwrap();
    let id = rig.approve(&view, &facts(true)).unwrap();
    assert_eq!(id, 3);
    let restored = GateSnapshot::from_store(&rig.store.body().unwrap().approved).unwrap();
    assert_eq!(
        restored.inputs[&layer("a", "user")].value,
        RecordedValue::new(json!(10))
    );
    assert_eq!(
        restored.inputs[&program("a")].value,
        RecordedValue::new(json!("program-v3-in-place"))
    );
    assert_eq!(
        rig.store.body().unwrap().generations[&id].kind,
        CommitKind::Rollback
    );
}

#[test]
fn approval_rollback_review_refusal_and_pruned_generation_create_no_plan() {
    let mut rig = Rig::new();
    rig.stamp();
    let error = rig
        .gate
        .rollback(&mut rig.store, 1, &rig.current, &rig.current, |_, _| {
            Err(GateError::new("bound_violated", "project-one"))
        })
        .unwrap_err();
    assert_eq!(error.code, "bound_violated");
    assert_eq!(rig.store.body().unwrap().last_generation, 1);
    for value in 11..37 {
        rig.edit(layer("a", "user"), json!(value));
        rig.edit(layer("b", "user"), json!(value));
        rig.edit(layer("callosum", "user"), json!(value));
        let view = rig.change(false);
        let generation = rig.approve(&view, &facts(true)).unwrap();
        let tuples: Vec<_> = rig
            .store
            .body()
            .unwrap()
            .versions
            .iter()
            .map(|(tuple, _)| tuple.clone())
            .collect();
        for tuple in tuples {
            rig.store.record_delivery(&tuple, generation, true).unwrap();
        }
    }
    assert_eq!(
        rig.gate
            .rollback(&mut rig.store, 1, &rig.current, &rig.current, complex)
            .unwrap_err()
            .code,
        "generation_pruned"
    );
}

#[test]
fn approval_recovery_unstamp_uses_fixed_os_sentence_without_a_phone() {
    let mut rig = Rig::new();
    rig.stamp();
    let file = std::fs::read_dir(&rig.path)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(file, b"tampered store").unwrap();
    assert!(rig.store.check().is_err());
    let view = rig
        .gate
        .plan(
            &mut rig.store,
            PlanKind::Unstamp,
            rig.current.clone(),
            &rig.current,
            complex,
        )
        .unwrap();
    let caller = facts(false);
    let challenge = rig
        .gate
        .begin_approval(&view.digest, &mut rig.store, &rig.current, &caller)
        .unwrap();
    assert!(challenge
        .prompt()
        .unwrap()
        .starts_with("Config files become authoritative again"));
    assert_eq!(
        rig.gate
            .finish_approval(
                challenge.attempt(),
                ApprovalReply::Presence(PresenceDecision::Unavailable),
                &mut rig.store,
                &rig.current,
                &caller,
                1
            )
            .unwrap_err()
            .code,
        "approval_surface_unavailable"
    );
    assert_eq!(rig.approve(&view, &caller).unwrap(), 1);
    assert_eq!(rig.store.status().state, StoreState::Unstamped);
}
