use subc_control::{ops, upgrade_in_place::*, ClientControlRequest};
use subc_protocol::ErrorBody;

#[test]
fn live_upgrade_request_matches_golden_bytes() {
    let golden = include_str!("golden/daemon_upgrade_in_place_request.json").trim_end();
    let request = ClientControlRequest::DaemonUpgradeInPlace {
        executable_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
    };
    assert_eq!(serde_json::to_string(&request).unwrap(), golden);
    assert_eq!(
        serde_json::from_str::<ClientControlRequest>(golden).unwrap(),
        request
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(golden).unwrap()["op"],
        ops::DAEMON_UPGRADE_IN_PLACE
    );
}

#[test]
fn live_upgrade_failure_matches_golden_bytes_and_codes() {
    let golden = include_str!("golden/error_body_upgrade_failed.json").trim_end();
    let error =
        ErrorBody::new(UPGRADE_FAILED, "daemon upgrade failed").with_detail(serde_json::json!({
            "announced": [],
            "attempt_id": "00112233445566778899aabbccddeeff",
            "restarted": ["worker"],
            "stage": "planned_stop",
        }));
    assert_eq!(serde_json::to_string(&error).unwrap(), golden);
    assert_eq!(serde_json::from_str::<ErrorBody>(golden).unwrap(), error);
    assert_eq!(UPGRADE_IN_PLACE_UNSUPPORTED, "upgrade_in_place_unsupported");
    assert_eq!(UPGRADE_EXECUTABLE_MISMATCH, "upgrade_executable_mismatch");
    assert_eq!(UPGRADE_SIGNATURE_MISMATCH, "upgrade_signature_mismatch");
    assert_eq!(UPGRADE_TARGET_INCOMPATIBLE, "upgrade_target_incompatible");
    assert_eq!(UPGRADE_BUSY, "upgrade_busy");
}
