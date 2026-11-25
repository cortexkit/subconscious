use std::fmt::Debug;

use serde::{de::DeserializeOwned, Serialize};
use subc_protocol::{
    error_codes,
    manifest::ModuleManifest,
    session::{ModuleControlCommand, LIVE_UPGRADE_CAPABILITY},
    ErrorBody, Flags, ModuleHelloAckBody, ModuleHelloBody, Priority, PROTOCOL_VERSION,
};

const TEST_TOKEN: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
const CAPABILITIES: &str = include_str!("golden/module_hello_body_with_module_capabilities.json");
const RESUME_HELLO: &str = include_str!("golden/module_hello_body_with_resume_token.json");
const RESUME_ACK: &str = include_str!("golden/module_hello_ack_body_with_resume_token.json");
const RESUME_REFUSED: &str = include_str!("golden/error_body_resume_refused.json");

fn hello() -> ModuleHelloBody {
    ModuleHelloBody {
        manifest: ModuleManifest::builder("upgrade-worker", "1.0.0").build(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
        module_capabilities: None,
        resume_token: None,
    }
}

fn ack() -> ModuleHelloAckBody {
    ModuleHelloAckBody {
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: vec![],
        subc_capabilities: vec![],
        storage: None,
        machine_id: None,
        resume_token: None,
    }
}

fn assert_wire<T: Debug + PartialEq + Serialize + DeserializeOwned>(body: T, golden: &str) {
    let wire = golden.trim_end().as_bytes();
    assert_eq!(serde_json::to_vec(&body).unwrap(), wire);
    assert_eq!(serde_json::from_slice::<T>(wire).unwrap(), body);
}

#[test]
fn live_upgrade_notice_matches_golden_bytes() {
    assert_wire(
        ModuleControlCommand::Upgrading {
            reconnect_by_ms: 1_764_000_000_000,
        },
        include_str!("golden/module_control_command_upgrading.json"),
    );
}

#[test]
fn live_upgrade_hello_matches_golden_bytes() {
    assert_wire(
        ModuleHelloBody {
            module_capabilities: Some(vec![LIVE_UPGRADE_CAPABILITY.to_owned()]),
            ..hello()
        },
        CAPABILITIES,
    );
    assert_wire(
        ModuleHelloBody {
            resume_token: Some(TEST_TOKEN.to_owned()),
            ..hello()
        },
        RESUME_HELLO,
    );
    assert_wire(
        ModuleHelloAckBody {
            resume_token: Some(TEST_TOKEN.to_owned()),
            ..ack()
        },
        RESUME_ACK,
    );
}

#[test]
fn live_upgrade_hello_and_ack_keep_their_old_bytes_and_readers() {
    use subc_protocol_published::{ModuleHelloAckBody as OldAck, ModuleHelloBody as OldHello};

    let baseline_hello: OldHello = serde_json::from_str(CAPABILITIES).unwrap();
    assert_eq!(baseline_hello.manifest.module_id, "upgrade-worker");
    let hello_bytes = serde_json::to_vec(&baseline_hello).unwrap();
    let baseline_resume_hello: OldHello = serde_json::from_str(RESUME_HELLO).unwrap();
    assert_eq!(
        serde_json::to_vec(&baseline_resume_hello).unwrap(),
        hello_bytes
    );
    assert_eq!(serde_json::to_vec(&hello()).unwrap(), hello_bytes);
    assert_eq!(
        serde_json::from_slice::<ModuleHelloBody>(&hello_bytes).unwrap(),
        hello()
    );
    let empty = ModuleHelloBody {
        module_capabilities: Some(vec![]),
        ..hello()
    };
    assert_eq!(serde_json::to_vec(&empty).unwrap(), hello_bytes);

    let baseline_ack: OldAck = serde_json::from_str(RESUME_ACK).unwrap();
    assert_eq!(baseline_ack.negotiated_ver, PROTOCOL_VERSION);
    let ack_bytes = serde_json::to_vec(&baseline_ack).unwrap();
    assert_eq!(serde_json::to_vec(&ack()).unwrap(), ack_bytes);
    assert_eq!(
        serde_json::from_slice::<ModuleHelloAckBody>(&ack_bytes).unwrap(),
        ack()
    );
}

#[test]
fn live_upgrade_token_is_on_the_wire_but_not_in_debug() {
    let hello: ModuleHelloBody = serde_json::from_str(RESUME_HELLO).unwrap();
    let ack: ModuleHelloAckBody = serde_json::from_str(RESUME_ACK).unwrap();
    for (wire, debug, pretty_debug) in [
        (
            serde_json::to_string(&hello).unwrap(),
            format!("{hello:?}"),
            format!("{hello:#?}"),
        ),
        (
            serde_json::to_string(&ack).unwrap(),
            format!("{ack:?}"),
            format!("{ack:#?}"),
        ),
    ] {
        assert!(wire.contains(TEST_TOKEN));
        for text in [debug, pretty_debug] {
            assert!(text.contains("<redacted>"));
            assert!(!text.contains(TEST_TOKEN), "Debug exposed the resume token");
        }
    }
}

#[test]
fn live_upgrade_refusals_preserve_retry_classes() {
    let refused = ErrorBody::new(error_codes::RESUME_REFUSED, "module resume refused");
    assert_wire(refused.clone(), RESUME_REFUSED);
    let flags = Flags::new(false, Priority::Passive, false).with_daemon_origin();
    assert!(!error_codes::is_retryable_resume_hello(&refused.code));
    assert!(!error_codes::is_retryable_resume_hello(
        "some_future_refusal"
    ));
    assert!(error_codes::is_retryable_resume_hello(
        "local_endpoint_clients_only"
    ));
    assert!(!error_codes::is_retryable_route_open_refusal(&refused));
    assert!(!error_codes::is_established_route_dead(
        flags,
        &refused.code
    ));
    assert!(!error_codes::is_route_busy_retryable(
        flags,
        &refused.code,
        true
    ));

    let reloading = ErrorBody::module_reloading_for_daemon_upgrade("daemon upgrade in progress");
    assert!(error_codes::is_retryable_route_open_refusal(&reloading));
    assert_wire(
        reloading,
        include_str!("golden/error_body_module_reloading_for_daemon_upgrade.json"),
    );
}
