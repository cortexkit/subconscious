use subc_protocol_published::{ModuleHelloAckBody, ModuleHelloBody, PROTOCOL_VERSION};

const BASELINE_HELLO: &str =
    include_str!("../../subc-protocol/tests/golden/module_hello_body_live_upgrade_baseline.json");
const BASELINE_ACK: &str = include_str!(
    "../../subc-protocol/tests/golden/module_hello_ack_body_live_upgrade_baseline.json"
);
const CAPABILITIES: &str = include_str!(
    "../../subc-protocol/tests/golden/module_hello_body_with_module_capabilities.json"
);
const RESUME_HELLO: &str =
    include_str!("../../subc-protocol/tests/golden/module_hello_body_with_resume_token.json");
const RESUME_ACK: &str =
    include_str!("../../subc-protocol/tests/golden/module_hello_ack_body_with_resume_token.json");

#[test]
fn live_upgrade_hello_and_ack_keep_their_old_bytes_and_readers() {
    // The published reader must drop the new optional fields and write the
    // same bytes as the current encoder does when the fields are absent.
    for wire in [BASELINE_HELLO, CAPABILITIES, RESUME_HELLO] {
        let hello: ModuleHelloBody = serde_json::from_str(wire).unwrap();
        assert_eq!(hello.manifest.module_id, "upgrade-worker");
        assert_eq!(
            serde_json::to_string(&hello).unwrap(),
            BASELINE_HELLO.trim_end()
        );
    }

    for wire in [BASELINE_ACK, RESUME_ACK] {
        let ack: ModuleHelloAckBody = serde_json::from_str(wire).unwrap();
        assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
        assert_eq!(
            serde_json::to_string(&ack).unwrap(),
            BASELINE_ACK.trim_end()
        );
    }
}
