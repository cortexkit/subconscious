use super::*;

fn hello_body(role_json: &str) -> Vec<u8> {
    format!(
            r#"{{"protocol_ver":2,"module_id":"m","manifest":{{"module_id":"m","module_version":"1.0.0","protocol_ver":2,"trust_tier":"first_party","provides":[{role_json}],"consumes":[],"bindings":{{"storage":{{"kind":"sqlite","scope":"project","owns_schema":false}},"vault_grants":[],"identity":{{"requires":[],"optional":[]}}}}}}}}"#
        )
        .into_bytes()
}

fn manifest_from(body: &[u8]) -> ModuleManifest {
    let value: serde_json::Value = serde_json::from_slice(body).expect("hello parses");
    serde_json::from_value(value.get("manifest").expect("manifest key").clone())
        .expect("manifest parses")
}

const SURFACE_TAIL: &str =
    r#""operations":[],"config_schema":{"type":"object"},"observability":[],"identity_scope":[]"#;

#[test]
fn absent_concurrency_on_management_surface_is_reported_as_defaulted() {
    let body = hello_body(&format!(
        r#"{{"role":"management_surface",{SURFACE_TAIL}}}"#
    ));
    let manifest = manifest_from(&body);
    // Precondition: serde really resolved it to the default, so the typed
    // manifest alone cannot answer the question this probe exists for.
    assert_eq!(manifest_concurrency(&manifest), Concurrency::ModuleManaged);
    assert!(manifest_concurrency_was_defaulted(&body, &manifest));
}

#[test]
fn declared_concurrency_is_not_reported_even_when_it_equals_the_default() {
    let body = hello_body(&format!(
        r#"{{"role":"management_surface",{SURFACE_TAIL},"concurrency":"module_managed"}}"#
    ));
    let manifest = manifest_from(&body);
    assert_eq!(manifest_concurrency(&manifest), Concurrency::ModuleManaged);
    assert!(!manifest_concurrency_was_defaulted(&body, &manifest));
}

#[test]
fn non_management_roles_are_never_reported() {
    let body = hello_body(
        r#"{"role":"internal_service","service_id":"s","transport":"bulk","agent_facing":false,"operations":[]}"#,
    );
    let manifest = manifest_from(&body);
    assert!(!manifest_concurrency_was_defaulted(&body, &manifest));
}
