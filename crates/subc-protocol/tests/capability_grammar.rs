use std::fs;

use serde_json::{json, Value};
use subc_protocol::manifest::{
    is_valid_capability_identifier, validate_manifest_capability_grammar,
};

#[test]
fn capability_grammar_vectors_pin_lexical_and_schema_refusals() {
    let vectors: Value = serde_json::from_str(
        &fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden/capability_grammar_validation.json"),
        )
        .expect("validation vectors are readable"),
    )
    .expect("validation vectors are JSON");

    for identifier in vectors["identifier_accepts"]
        .as_array()
        .expect("acceptance vector is an array")
    {
        let identifier = identifier.as_str().expect("identifier is a string");
        assert!(
            is_valid_capability_identifier(identifier),
            "accepted identifier was refused: {identifier}"
        );
    }
    for identifier in vectors["identifier_rejects"]
        .as_array()
        .expect("rejection vector is an array")
    {
        let identifier = identifier.as_str().expect("identifier is a string");
        assert!(
            !is_valid_capability_identifier(identifier),
            "rejected identifier was accepted: {identifier}"
        );
    }

    validate_manifest_capability_grammar(&json!({
        "runtime_computed": [vectors["runtime_computed_legal"].clone()]
    }))
    .expect("RFC 6901 pointer outside capabilities is legal");

    for vector in vectors["refusals"]
        .as_array()
        .expect("refusal vector is an array")
    {
        let mut manifest = json!({});
        if let Some(capabilities) = vector.get("capabilities") {
            manifest["capabilities"] = capabilities.clone();
        }
        if let Some(runtime_computed) = vector.get("runtime_computed") {
            manifest["runtime_computed"] = runtime_computed.clone();
        }
        let error = validate_manifest_capability_grammar(&manifest)
            .expect_err("pinned malformed declaration must be refused");
        assert_eq!(error.field(), vector["field"].as_str().unwrap());
        assert_eq!(error.value(), vector["value"].as_str().unwrap());
    }
}
