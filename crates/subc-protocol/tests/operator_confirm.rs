use serde_json::{json, Value};
use subc_protocol::{
    error_codes,
    session::{OperatorConfirmReply, OperatorConfirmRequest},
};

#[test]
fn operator_confirm_request_has_the_wire_shape_and_round_trips() {
    let request = OperatorConfirmRequest::new("Retire the agent", 7, 42);
    let wire = r#"{"op":"operator.confirm","summary":"Retire the agent","route_channel":7,"route_epoch":42}"#;
    assert_eq!(serde_json::to_string(&request).unwrap(), wire);
    assert_eq!(
        serde_json::from_str::<OperatorConfirmRequest>(wire).unwrap(),
        request
    );
}

#[test]
fn operator_confirm_reply_has_the_wire_shape_and_round_trips() {
    let reply = OperatorConfirmReply::confirmed();
    let wire = r#"{"op":"operator.confirm","outcome":"confirmed"}"#;
    assert_eq!(serde_json::to_string(&reply).unwrap(), wire);
    assert_eq!(
        serde_json::from_str::<OperatorConfirmReply>(wire).unwrap(),
        reply
    );
}

#[test]
fn operator_confirm_request_requires_the_op_and_all_fields() {
    let valid = json!({
        "op": "operator.confirm",
        "summary": "Retire the agent",
        "route_channel": 7,
        "route_epoch": 42
    });
    for field in ["op", "summary", "route_channel", "route_epoch"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<OperatorConfirmRequest>(missing).is_err(),
            "missing {field} must not decode"
        );
    }
    for invalid_op in [json!("scope.describe"), Value::Null, json!(7)] {
        let mut invalid = valid.clone();
        invalid["op"] = invalid_op;
        assert!(serde_json::from_value::<OperatorConfirmRequest>(invalid).is_err());
    }
    assert!(serde_json::from_value::<OperatorConfirmRequest>(json!({
        "op": "operator.confirm"
    }))
    .is_err());
}

#[test]
fn operator_confirm_reply_requires_the_op_and_outcome() {
    for invalid in [
        json!({"op": "operator.confirm"}),
        json!({"outcome": "confirmed"}),
        json!({"op": "scope.describe", "outcome": "confirmed"}),
        json!({"op": "operator.confirm", "outcome": null}),
    ] {
        assert!(serde_json::from_value::<OperatorConfirmReply>(invalid).is_err());
    }
}

#[test]
fn operator_confirm_bodies_allow_additive_unknown_fields() {
    let request: OperatorConfirmRequest = serde_json::from_value(json!({
        "op": "operator.confirm",
        "summary": "Retire the agent",
        "route_channel": 7,
        "route_epoch": 42,
        "future_field": true
    }))
    .unwrap();
    assert_eq!(
        request,
        OperatorConfirmRequest::new("Retire the agent", 7, 42)
    );
    let reply: OperatorConfirmReply = serde_json::from_value(json!({
        "op": "operator.confirm",
        "outcome": "confirmed",
        "future_field": true
    }))
    .unwrap();
    assert_eq!(reply, OperatorConfirmReply::confirmed());
}

#[test]
fn operator_confirm_error_codes_are_terminal_and_not_route_open_refusals() {
    let table: Value = serde_json::from_str(include_str!("golden/decision_tables.json")).unwrap();
    let route_open = table["route_open_retryable"].as_object().unwrap();
    for (code, spelling) in [
        (error_codes::OPERATOR_DECLINED, "operator_declined"),
        (
            error_codes::OPERATOR_PRESENCE_UNAVAILABLE,
            "operator_presence_unavailable",
        ),
        (
            error_codes::OPERATOR_SUMMARY_INVALID,
            "operator_summary_invalid",
        ),
        (
            error_codes::OPERATOR_REQUEST_NOT_PERMITTED,
            "operator_request_not_permitted",
        ),
    ] {
        assert_eq!(code, spelling);
        assert!(
            !error_codes::is_retryable_route_open(code),
            "{code} must not be retried"
        );
        assert!(
            !route_open.contains_key(code),
            "{code} is not a route.open refusal"
        );
    }
}
