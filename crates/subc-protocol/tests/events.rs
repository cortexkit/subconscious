use serde_json::{json, Value};
use subc_protocol::manifest::{
    validate_hello_event_declarations, EventDeclaration, ModuleManifest,
};

#[test]
fn event_declarations_round_trip_against_golden() {
    let manifest = ModuleManifest::builder("plexus", "1.0.0")
        .protocol_ver(2)
        .events(Some(vec![
            EventDeclaration::new("github_ci_job_failed", 1)
                .with_headers(
                    ["repo", "branch", "pr", "conclusion"]
                        .map(String::from)
                        .to_vec(),
                )
                .with_summary(Some("A GitHub CI job failed.".into())),
            EventDeclaration::new("github_ci_job_failed", 2),
        ]))
        .build();
    let golden = include_str!("golden/module_manifest_with_events.json").trim_end();
    assert_eq!(serde_json::to_string(&manifest).unwrap(), golden);
    assert_eq!(
        serde_json::from_str::<ModuleManifest>(golden).unwrap(),
        manifest
    );
}

#[test]
fn event_absence_and_empty_declaration_remain_distinct_on_wire() {
    let manifest = ModuleManifest::builder("legacy", "1.0.0")
        .protocol_ver(2)
        .build();
    let absent =
        r#"{"module_id":"legacy","module_version":"1.0.0","protocol_ver":2,"provides":[]}"#;
    assert_eq!(serde_json::to_string(&manifest).unwrap(), absent);
    assert!(serde_json::from_str::<ModuleManifest>(absent)
        .unwrap()
        .events
        .is_none());
    let empty = ModuleManifest::builder("legacy", "1.0.0")
        .protocol_ver(2)
        .events(Some(vec![]))
        .build();
    let empty_wire = r#"{"module_id":"legacy","module_version":"1.0.0","protocol_ver":2,"provides":[],"events":[]}"#;
    assert_eq!(serde_json::to_string(&empty).unwrap(), empty_wire);
    assert_eq!(
        serde_json::from_str::<ModuleManifest>(empty_wire)
            .unwrap()
            .events,
        Some(vec![])
    );
}

fn hello(events: Value) -> Value {
    json!({"manifest": {"module_id": "events", "module_version": "1.0.0", "protocol_ver": 2, "provides": [], "events": events}})
}

#[test]
fn event_name_validation_table_matches_bus_token_rule() {
    // These valid and invalid sets must agree with cortexkit-bus-naming 0.2.0's
    // TokenKind::EventName rule (token.rs:91-143), not its wider module-id rule.
    let valid = [
        "a".to_string(),
        "0".into(),
        "github_ci_job_failed".into(),
        "a_0".into(),
        "a".repeat(63),
    ];
    let invalid = [
        "".to_string(),
        "_event".into(),
        "Event".into(),
        "eVent".into(),
        "event.name".into(),
        "event-name".into(),
        "*".into(),
        ">".into(),
        "event*".into(),
        "event>".into(),
        " event".into(),
        "event ".into(),
        "event\n".into(),
        "évent".into(),
        "a".repeat(64),
    ];
    for (names, expected) in [(valid.as_slice(), true), (invalid.as_slice(), false)] {
        for name in names {
            let body = hello(json!([{"name": name, "version": 1, "headers": []}]));
            let result = validate_hello_event_declarations(&body);
            assert_eq!(result.is_ok(), expected, "event name {name:?}");
            if let Err(error) = result {
                assert_eq!(error.field(), "events[0].name");
            }
        }
    }
}

#[test]
fn event_validation_accepts_exact_bounds_and_tolerates_unknown_fields() {
    let entries: Vec<_> = (0..64)
        .map(|version| {
            json!({
                "name": "a".repeat(63), "version": version + 1,
                "headers": (0..16).map(|index| format!("a{index:031}")).collect::<Vec<_>>(),
                "summary": "é".repeat(200), "future_hint": true
            })
        })
        .collect();
    let body = hello(json!(entries));
    validate_hello_event_declarations(&body).unwrap();
    let manifest: ModuleManifest = serde_json::from_value(body["manifest"].clone()).unwrap();
    assert_eq!(manifest.events.unwrap().len(), 64);
    validate_hello_event_declarations(&hello(
        json!([{"name":"a", "version":u32::MAX,"headers":[],"summary":null}]),
    ))
    .unwrap();
    // Null is the existing Option-field spelling of absence, not an empty declaration.
    assert!(
        serde_json::from_value::<ModuleManifest>(hello(Value::Null)["manifest"].clone())
            .unwrap()
            .events
            .is_none()
    );
}

#[test]
fn manifest_decode_enforces_event_validation_with_field_context() {
    for (events, field) in [
        (json!({}), "events"),
        (
            json!([{"name":"a", "version":0, "headers":[]}]),
            "events[0].version",
        ),
        (
            json!([{"name":"a", "version":1, "headers":["Repo"]}]),
            "events[0].headers[0]",
        ),
        (
            json!([{"name":"a", "version":1, "headers":[], "summary":"a\n"}]),
            "events[0].summary",
        ),
    ] {
        let error = serde_json::from_value::<ModuleManifest>(hello(events)["manifest"].clone())
            .unwrap_err();
        assert!(error.to_string().contains(field), "{error}");
    }
}
