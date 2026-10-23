use subc_control::ClientControlResponse;

#[test]
fn supervisor_ack_keeps_published_literal_pattern_and_wire_bytes() {
    let response = ClientControlResponse::SupervisorAck {
        module_id: "provider".into(),
        applied: true,
    };
    let bytes = r#"{"op":"supervisor.ack","module_id":"provider","applied":true}"#;
    assert_eq!(serde_json::to_string(&response).unwrap(), bytes);
    assert_eq!(
        serde_json::from_str::<ClientControlResponse>(bytes).unwrap(),
        response
    );
    match response {
        ClientControlResponse::SupervisorAck { module_id, applied } => {
            assert_eq!(module_id, "provider");
            assert!(applied);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}
