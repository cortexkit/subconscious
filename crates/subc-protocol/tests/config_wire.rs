use std::{collections::BTreeMap, fmt::Debug, fs, path::PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use subc_config::{
    ApplyKind, Changeset, FieldChange, InputState, InputVector, Plan, RecordedValue,
};
use subc_protocol::{config::*, consent::*, error_codes, ErrorBody};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/config_gate")
        .join(format!("{name}.json"));
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn golden<T: Serialize + DeserializeOwned + PartialEq + Debug>(name: &str, expected: &T) {
    let value = fixture(name);
    assert_eq!(serde_json::to_value(expected).unwrap(), value, "{name}");
    assert_eq!(
        serde_json::from_value::<T>(value).unwrap(),
        *expected,
        "{name}"
    );
}

fn file(bytes: &[u8]) -> ConfigLayerSnapshot {
    ConfigLayerSnapshot::file(
        "sample",
        ConfigLayer::User,
        "/config/sample.jsonc",
        Some(bytes.to_vec()),
    )
}

fn missing() -> ConfigLayerSnapshot {
    let mut value = ConfigLayerSnapshot::file(
        "sample",
        ConfigLayer::Project,
        "/projects/a/.cortexkit/sample.jsonc",
        None,
    );
    value.project_id = Some("project-a".into());
    value.workspace_id = Some("workspace-a".into());
    value
}

fn active() -> ConfigView {
    ConfigView::new(
        3,
        json!({"limit": 10}),
        BTreeMap::from([("/limit".into(), Layer::User)]),
        vec![file(b"\xef\xbb\xbf{\"limit\":10}\r\n"), missing()],
        ConfigStatus::Active,
    )
}

fn pending() -> ConfigView {
    let mut view = ConfigView::new(
        4,
        json!({"limit": 20}),
        BTreeMap::from([("/limit".into(), Layer::User)]),
        vec![file(b"{\"limit\":20}\r\n"), missing()],
        ConfigStatus::Pending,
    );
    view.attempt = Some(8);
    view
}

#[test]
fn config_wire_snapshots_pin_exact_bom_crlf_missing_and_empty_bytes() {
    golden(
        "config_snapshot_bom_crlf",
        &file(b"\xef\xbb\xbf{\"limit\":10}\r\n"),
    );
    golden("config_snapshot_missing", &missing());
    golden("config_snapshot_empty", &file(b""));
    let decoded: ConfigLayerSnapshot =
        serde_json::from_value(fixture("config_snapshot_bom_crlf")).unwrap();
    assert_eq!(
        decoded.bytes.as_deref(),
        Some(b"\xef\xbb\xbf{\"limit\":10}\r\n".as_slice())
    );
    assert_ne!(missing().bytes, file(b"").bytes);
}

#[test]
fn config_wire_snapshots_refuse_digest_mismatch_and_incomplete_content() {
    let error =
        serde_json::from_value::<ConfigLayerSnapshot>(fixture("config_snapshot_digest_mismatch"))
            .unwrap_err();
    assert!(
        error.to_string().contains("digest: does not match bytes"),
        "{error}"
    );
    for (remove, field) in [("bytes", "bytes"), ("digest", "digest")] {
        let mut value = fixture("config_snapshot_bom_crlf");
        value.as_object_mut().unwrap().remove(remove);
        let error = serde_json::from_value::<ConfigLayerSnapshot>(value).unwrap_err();
        assert!(error.to_string().contains(field), "{error}");
    }
}

#[test]
fn config_wire_snapshots_refuse_noncanonical_base64() {
    // Each spelling either omits required padding, has nonzero trailing bits,
    // adds whitespace, uses URL-safe alphabet, or adds excess padding.
    for spelling in [
        "77u/eyJsaW1pdCI6MTB9DQo",
        "77u/eyJsaW1pdCI6MTB9DQp=",
        "77u/eyJsaW1pdCI6MTB9DQo=\n",
        "77u_eyJsaW1pdCI6MTB9DQo=",
        "77u/eyJsaW1pdCI6MTB9DQo==",
    ] {
        let mut value = fixture("config_snapshot_bom_crlf");
        value["bytes"] = spelling.into();
        let error = serde_json::from_value::<ConfigLayerSnapshot>(value).unwrap_err();
        assert!(
            error.to_string().contains("bytes:"),
            "{spelling:?}: {error}"
        );
    }
}

#[test]
fn config_wire_review_carries_immutable_base_candidate_and_cross_field_refusals() {
    golden(
        "config_review_request",
        &ConfigReviewRequest::new(3, pending().layers, Some(active())),
    );
    let mut field = FieldChange::new(
        "sample",
        "/limit",
        RecordedValue::new(json!(10)),
        RecordedValue::new(json!(20)),
        ApplyKind::Live,
    );
    field.old_source = Layer::User;
    field.new_source = Layer::Project;
    field.reasons = vec!["Project limit raised".into()];
    let mut refusal = ConfigReviewRefusal::new(
        error_codes::BOUND_VIOLATED,
        vec!["/limit".into(), "/ceiling".into()],
        vec!["Limit exceeds ceiling".into()],
    );
    refusal.project_id = Some("project-a".into());
    golden(
        "config_review_reply",
        &ConfigReviewReply::new(vec![field], vec![refusal]),
    );

    let request = ConfigReviewRequest::new(0, vec![], None);
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({"op":"config.review","base_generation":0,"layers":[],"base":null})
    );
}

#[test]
fn config_wire_declaration_is_separate_from_manifest() {
    let request = ConfigDeclareRequest::new(
        json!({"type":"object","properties":{"limit":{
        "type":"integer","description":"Work limit","default":5,"x-ck-apply":"live",
        "x-ck-layers":["user","project"]}},"unevaluatedProperties":false}),
        "/config/sample.jsonc",
        vec!["/projects/a".into()],
    );
    golden("config_declare_request", &request);
    golden("config_declare_reply", &ConfigDeclareReply::new());
    assert_eq!(CONFIG_REVIEW_OP, "config.review");
    assert_eq!(CONFIG_GATE_CAPABILITY, "config.gate");
    let error = serde_json::from_value::<subc_protocol::manifest::CapabilityDeclarations>(
        json!({"config_gate":true}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown field"), "{error}");
}

#[test]
fn config_wire_get_separates_active_pending_and_first_delivery() {
    golden("config_get_request", &ConfigGetRequest::new());
    let mut request = ConfigGetRequest::new();
    request.project_id = Some("project-a".into());
    request.generation = Some(4);
    request.attempt = Some(8);
    golden("config_get_request_assigned", &request);
    golden(
        "config_get_reply_active",
        &ConfigGetReply::new(Some(active()), None),
    );
    golden(
        "config_get_reply_pending",
        &ConfigGetReply::new(Some(active()), Some(pending())),
    );
    golden(
        "config_get_reply_first_delivery",
        &ConfigGetReply::new(None, Some(pending())),
    );
    let view: ConfigGetReply = serde_json::from_value(fixture("config_get_reply_pending")).unwrap();
    assert_eq!(view.active.unwrap().generation, 3);
    assert_eq!(view.pending.unwrap().generation, 4);
}

#[test]
fn config_wire_no_approved_view_is_an_error_not_empty_config() {
    golden(
        "config_get_error_gated",
        &ErrorBody::new(
            error_codes::CONFIG_GATED,
            "No approved configuration is available",
        ),
    );
    assert!(serde_json::from_value::<ConfigGetReply>(fixture("config_get_error_gated")).is_err());
}

#[test]
fn config_wire_applied_and_changed_golden() {
    let ack = ConfigAppliedRequest::new(4, 8, ConfigApplyResult::Ack, vec![]);
    assert!(ack.result.is_ack());
    golden("config_applied_request_ack", &ack);
    let mut nack = ConfigAppliedRequest::new(
        4,
        8,
        ConfigApplyResult::Nack,
        vec!["Cannot activate this generation".into()],
    );
    nack.project_id = Some("project-a".into());
    assert!(!nack.result.is_ack());
    golden("config_applied_request_nack", &nack);
    golden("config_applied_reply", &ConfigAppliedReply::new());
    let mut changed = ConfigChanged::new(4, vec!["/limit".into()]);
    changed.project_id = Some("project-a".into());
    golden("config_changed", &changed);
}

#[test]
fn config_wire_unknown_result_layer_and_status_never_grant() {
    let request = ConfigAppliedRequest::new(
        4,
        8,
        ConfigApplyResult::Unknown("future_result".into()),
        vec![],
    );
    golden("config_applied_request_unknown", &request);
    let decoded: ConfigAppliedRequest =
        serde_json::from_value(fixture("config_applied_request_unknown")).unwrap();
    assert!(!decoded.result.is_ack());

    let mut snapshot = file(b"\xef\xbb\xbf{\"limit\":10}\r\n");
    snapshot.layer = ConfigLayer::Unknown("future_layer".into());
    golden("config_snapshot_unknown_layer", &snapshot);
    let decoded: ConfigLayerSnapshot =
        serde_json::from_value(fixture("config_snapshot_unknown_layer")).unwrap();
    assert_eq!(decoded.layer.known_layer(), None);

    let mut view = active();
    view.status = ConfigStatus::Unknown("future_status".into());
    golden(
        "config_get_reply_unknown_status",
        &ConfigGetReply::new(Some(view), None),
    );
    let decoded: ConfigGetReply =
        serde_json::from_value(fixture("config_get_reply_unknown_status")).unwrap();
    assert!(!decoded.active.unwrap().status.is_active());
}

fn extensible_fixed_tag<T: Serialize + DeserializeOwned + PartialEq + Debug>(
    name: &str,
    key: &str,
) {
    let original = fixture(name);
    let expected: T = serde_json::from_value(original.clone()).unwrap();
    let mut extended = original.clone();
    extended["future_extension"] = json!({"ignored":true});
    assert_eq!(
        serde_json::from_value::<T>(extended).unwrap(),
        expected,
        "{name}"
    );
    let mut wrong = original.clone();
    wrong[key] = "future.operation".into();
    assert!(
        serde_json::from_value::<T>(wrong).is_err(),
        "{name}: wrong tag accepted"
    );
    let mut missing = original;
    missing.as_object_mut().unwrap().remove(key);
    assert!(
        serde_json::from_value::<T>(missing).is_err(),
        "{name}: missing tag accepted"
    );
}

#[test]
fn config_wire_readers_ignore_extensions_but_require_operation_tags() {
    extensible_fixed_tag::<ConfigReviewRequest>("config_review_request", "op");
    extensible_fixed_tag::<ConfigReviewReply>("config_review_reply", "op");
    extensible_fixed_tag::<ConfigDeclareRequest>("config_declare_request", "op");
    extensible_fixed_tag::<ConfigDeclareReply>("config_declare_reply", "op");
    extensible_fixed_tag::<ConfigGetRequest>("config_get_request", "op");
    extensible_fixed_tag::<ConfigGetReply>("config_get_reply_pending", "op");
    extensible_fixed_tag::<ConfigAppliedRequest>("config_applied_request_ack", "op");
    extensible_fixed_tag::<ConfigAppliedReply>("config_applied_reply", "op");
    extensible_fixed_tag::<ConfigChanged>("config_changed", "op");
    let mut snapshot = fixture("config_snapshot_bom_crlf");
    snapshot["future_metadata"] = true.into();
    assert_eq!(
        serde_json::from_value::<ConfigLayerSnapshot>(snapshot).unwrap(),
        file(b"\xef\xbb\xbf{\"limit\":10}\r\n")
    );
}

#[test]
fn config_wire_pending_is_optional_for_readers_and_absent_for_old_views() {
    #[derive(Deserialize)]
    struct ActiveOnlyReader {
        active: Option<ConfigView>,
    }
    let legacy: ActiveOnlyReader =
        serde_json::from_value(fixture("config_get_reply_pending")).unwrap();
    assert_eq!(legacy.active.unwrap(), active());
    let current: ConfigGetReply =
        serde_json::from_value(fixture("config_get_reply_active")).unwrap();
    assert!(current.pending.is_none());
    assert!(!serde_json::to_value(current)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("pending"));
}

fn consent_request() -> ConsentRequest {
    let mut changeset = Changeset::new();
    changeset.apply_plan = json!({"kind":"live"});
    let mut plan = Plan::new(
        7,
        3,
        "test-daemon",
        InputVector::new(BTreeMap::from([("subc.jsonc".into(), InputState::Absent)])),
        changeset,
    );
    plan.active_generations.insert("sample".into(), 3);
    ConsentRequest::new(plan).unwrap()
}

#[test]
fn config_wire_consent_v1_requester_and_responder_decode_shared_golden() {
    let requester = consent_request();
    golden("consent_v1_request", &requester);
    let responder_input: ConsentRequest =
        serde_json::from_value(fixture("consent_v1_request")).unwrap();
    assert!(responder_input.is_consistent());
    let response = ConsentResponse::new(&responder_input, ConsentDecision::Approved);
    golden("consent_v1_response_approved", &response);
    let requester_input: ConsentResponse =
        serde_json::from_value(fixture("consent_v1_response_approved")).unwrap();
    assert!(requester_input.approves(&requester));
    golden(
        "consent_v1_response_declined",
        &ConsentResponse::new(&requester, ConsentDecision::Declined),
    );
    assert!(!ConsentResponse::new(&requester, ConsentDecision::Declined).approves(&requester));
    assert_eq!(CONSENT_V1, "consent/v1");
    extensible_fixed_tag::<ConsentRequest>("consent_v1_request", "protocol");
    extensible_fixed_tag::<ConsentResponse>("consent_v1_response_approved", "protocol");
}

#[test]
fn config_wire_consent_v1_unknown_decisions_and_wrong_bindings_never_approve() {
    let request = consent_request();
    let unknown =
        ConsentResponse::new(&request, ConsentDecision::Unknown("future_decision".into()));
    golden("consent_v1_response_unknown", &unknown);
    let decoded: ConsentResponse =
        serde_json::from_value(fixture("consent_v1_response_unknown")).unwrap();
    assert!(!decoded.decision.is_approved());
    assert!(!decoded.approves(&request));
    let valid = ConsentResponse::new(&request, ConsentDecision::Approved);
    let mut wrong_digest = valid.clone();
    wrong_digest.plan_digest = subc_config::raw_digest(b"another plan");
    assert!(!wrong_digest.approves(&request));
    let mut wrong_revision = valid.clone();
    wrong_revision.base_revision += 1;
    assert!(!wrong_revision.approves(&request));
    let mut wrong_vector = valid;
    wrong_vector.input_vector.inputs.clear();
    assert!(!wrong_vector.approves(&request));
}

#[test]
fn config_wire_consent_v1_request_binding_must_describe_its_plan() {
    let request = consent_request();
    assert!(request.is_consistent());
    let mut other_digest = request.clone();
    other_digest.plan_digest = subc_config::raw_digest(b"another plan");
    assert!(!other_digest.is_consistent());
    let mut other_revision = request.clone();
    other_revision.base_revision += 1;
    assert!(!other_revision.is_consistent());
    let mut other_vector = request;
    other_vector.input_vector.inputs.clear();
    assert!(!other_vector.is_consistent());
}

#[test]
fn config_wire_shared_refusal_codes_have_exact_intent_spellings() {
    let codes = [
        error_codes::CONFIG_GATED,
        error_codes::PLAN_STALE,
        error_codes::LAYER_FORBIDDEN,
        error_codes::BOUND_VIOLATED,
        error_codes::VALUE_INVALID,
        error_codes::CREDENTIAL_SHARED,
        error_codes::DECLARATION_ORDER,
        error_codes::PROGRAM_REFUSED,
        error_codes::STORE_TAMPERED,
        error_codes::ANCHOR_PENDING,
        error_codes::GENERATION_PRUNED,
        error_codes::APPROVAL_SURFACE_UNAVAILABLE,
        error_codes::APPROVAL_DECLINED,
        error_codes::NOT_DELIVERY_TARGET,
    ];
    assert_eq!(
        codes,
        [
            "config_gated",
            "plan_stale",
            "layer_forbidden",
            "bound_violated",
            "value_invalid",
            "credential_shared",
            "declaration_order",
            "program_refused",
            "store_tampered",
            "anchor_pending",
            "generation_pruned",
            "approval_surface_unavailable",
            "approval_declined",
            "not_delivery_target"
        ]
    );
}
