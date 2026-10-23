use serde_json::{json, Value};
use subc_protocol::{
    framed_body::{self, FramedBodyError},
    manifest::{ModuleManifest, OperationPayload},
};

fn vectors() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vectors/framed-body-v1")
}

#[test]
fn shared_framed_body_vectors() {
    let cases: Value =
        serde_json::from_slice(&std::fs::read(vectors().join("cases.json")).unwrap()).unwrap();
    for case in cases["bodies"].as_array().unwrap() {
        let name = case["file"].as_str().unwrap();
        let body = std::fs::read(vectors().join(name)).unwrap();
        match framed_body::decode(&body) {
            Ok((head, payload)) => {
                assert!(case.get("error").is_none(), "{name}");
                assert_eq!(head, case["head"], "{name}");
                let hex: String = payload.iter().map(|byte| format!("{byte:02x}")).collect();
                assert_eq!(hex, case["payload_hex"].as_str().unwrap(), "{name}");
                assert_eq!(framed_body::encode(&head, payload).unwrap(), body, "{name}");
                assert_eq!(
                    payload.as_ptr(),
                    body[body.len() - payload.len()..].as_ptr()
                );
            }
            Err(error) => assert_eq!(
                format!("{error:?}"),
                case["error"].as_str().unwrap(),
                "{name}"
            ),
        }
    }
}

#[test]
fn head_longer_than_body_is_refused() {
    assert_eq!(
        framed_body::decode(b"\0\0\0\x03{}"),
        Err(FramedBodyError::HeadLongerThanBody)
    );
}

#[test]
fn encoder_refuses_nonobject_and_oversized_heads() {
    assert_eq!(
        framed_body::encode(&json!([]), &[]),
        Err(FramedBodyError::HeadNotObject)
    );
    assert_eq!(
        framed_body::encode(&json!({"padding": "x".repeat(65_536)}), &[]),
        Err(FramedBodyError::HeadTooLarge)
    );
    for len in 0..4 {
        assert_eq!(
            framed_body::decode(&[0; 4][..len]),
            Err(FramedBodyError::ShortBody)
        );
    }
}

#[test]
fn shared_operation_payload_vectors() {
    let cases: Value =
        serde_json::from_slice(&std::fs::read(vectors().join("cases.json")).unwrap()).unwrap();
    for case in cases["manifests"].as_array().unwrap() {
        let name = case["file"].as_str().unwrap();
        let bytes = std::fs::read(vectors().join(name)).unwrap();
        match serde_json::from_slice::<ModuleManifest>(&bytes) {
            Ok(manifest) => {
                assert!(case["error"].is_null(), "{name}");
                manifest.validate_operation_payload().unwrap();
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(serde_json::to_value(&manifest).unwrap(), value, "{name}");
                assert!(manifest.operation_payload["chunk"].max_bytes() > 0);
            }
            Err(error) => {
                assert!(!case["error"].is_null(), "{name}: {error}");
                assert!(
                    error.to_string().contains(case["error"].as_str().unwrap()),
                    "{name}: {error}"
                );
            }
        }
    }
}

#[test]
fn payload_map_is_additive_and_omitted_when_empty() {
    let mut manifest = ModuleManifest::builder("legacy", "1")
        .operation_payload(Default::default())
        .build();
    let encoded = serde_json::to_vec(&manifest).unwrap();
    assert_eq!(
        encoded,
        br#"{"module_id":"legacy","module_version":"1","protocol_ver":2,"provides":[]}"#
    );
    assert_eq!(
        serde_json::from_slice::<ModuleManifest>(&encoded).unwrap(),
        manifest
    );
    manifest
        .operation_payload
        .insert("missing".into(), OperationPayload::new(1));
    assert!(manifest.validate_operation_payload().is_err());
}
