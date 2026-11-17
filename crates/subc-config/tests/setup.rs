use serde_json::{json, Value};
use subc_config::{RefusalCode, Schema};

fn field(kind: &str, default: Value) -> Value {
    json!({"type":kind,"description":"Example field","default":default,
        "x-ck-layers":["user"],"x-ck-apply":"live"})
}

fn object(properties: Value) -> Value {
    json!({"type":"object","properties":properties,"unevaluatedProperties":false})
}

fn fixture() -> Value {
    let mut mode = field("string", json!("off"));
    mode["enum"] = json!(["off", "brief"]);
    let mut journal = object(json!({
        "enabled":field("boolean", json!(false)),
        "nested":object(json!({"mode":mode})),
        "a/b~c":field("boolean", json!(false))
    }));
    journal["title"] = json!("Journal reads");
    journal["description"] = json!("Read a journal's entries.");
    journal["x-ck-setup"] = json!({"feature":"journal_reads","order":-2,
        "recommended":{"/journal/enabled":true,"/journal/nested/mode":"brief",
            "/journal/a~1b~0c":true}});
    let mut archive = object(json!({"enabled":field("boolean", json!(false))}));
    archive["x-ck-setup"] = json!({"feature":"archive_push","order":-2,"recommended":{}});
    let mut root = object(json!({"journal":journal,"archive":archive}));
    root["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
    root
}

fn refused(schema: Value, path: &str) {
    fn remove_setup(node: &mut Value) {
        match node {
            Value::Object(object) => {
                object.remove("x-ck-setup");
                for child in object.values_mut() {
                    remove_setup(child);
                }
            }
            Value::Array(array) => array.iter_mut().for_each(remove_setup),
            _ => {}
        }
    }
    let mut without_setup = schema.clone();
    remove_setup(&mut without_setup);
    Schema::new(without_setup).expect("the schema is valid without setup annotations");
    let error = Schema::new(schema).expect_err("schema must be refused");
    assert_eq!(error.code, RefusalCode::SchemaInvalid);
    assert_eq!(error.path, path);
}

fn recommended(target: &str, value: Value) -> Value {
    let mut schema = fixture();
    schema["properties"]["journal"]["x-ck-setup"]["recommended"] = json!({target:value});
    schema
}

#[test]
fn setup_accepts_two_sections_without_applying_recommendations() {
    let declared = fixture();
    let schema = Schema::new(declared.clone()).expect("two setup sections are valid");
    for section in ["journal", "archive"] {
        assert_eq!(
            schema.normalized()["properties"][section]["x-ck-setup"],
            declared["properties"][section]["x-ck-setup"]
        );
    }
    assert_eq!(schema.defaults()["journal"]["enabled"], false);
    assert_eq!(schema.defaults()["journal"]["nested"]["mode"], "off");
}

#[test]
fn setup_refuses_non_object() {
    for value in [json!(false), json!([]), json!("feature"), Value::Null] {
        let mut schema = fixture();
        schema["properties"]["journal"]["x-ck-setup"] = value;
        refused(schema, "/journal");
    }
}

#[test]
fn setup_refuses_missing_feature() {
    let mut schema = fixture();
    schema["properties"]["journal"]["x-ck-setup"]
        .as_object_mut()
        .unwrap()
        .remove("feature");
    refused(schema, "/journal");
}

#[test]
fn setup_refuses_malformed_feature() {
    for value in [
        json!(false),
        json!(""),
        json!("Upper"),
        json!("has-dash"),
        json!("a\n"),
        json!("a".repeat(65)),
        json!("é"),
    ] {
        let mut schema = fixture();
        schema["properties"]["journal"]["x-ck-setup"]["feature"] = value;
        refused(schema, "/journal");
    }
}

#[test]
fn setup_refuses_duplicate_feature() {
    let mut schema = fixture();
    schema["properties"]["journal"]["x-ck-setup"]["feature"] = json!("archive_push");
    refused(schema, "/journal");
}

#[test]
fn setup_refuses_missing_order() {
    let mut schema = fixture();
    schema["properties"]["journal"]["x-ck-setup"]
        .as_object_mut()
        .unwrap()
        .remove("order");
    refused(schema, "/journal");
}

#[test]
fn setup_refuses_non_integer_order() {
    for value in [
        json!(1.5),
        json!(10.0),
        json!("10"),
        json!(true),
        Value::Null,
    ] {
        let mut schema = fixture();
        schema["properties"]["journal"]["x-ck-setup"]["order"] = value;
        refused(schema, "/journal");
    }
}

#[test]
fn setup_refuses_missing_recommended() {
    let mut schema = fixture();
    schema["properties"]["journal"]["x-ck-setup"]
        .as_object_mut()
        .unwrap()
        .remove("recommended");
    refused(schema, "/journal");
}

#[test]
fn setup_refuses_non_object_recommended() {
    for value in [json!([]), json!(false), Value::Null] {
        let mut schema = fixture();
        schema["properties"]["journal"]["x-ck-setup"]["recommended"] = value;
        refused(schema, "/journal");
    }
}

#[test]
fn setup_refuses_extra_member() {
    let mut schema = fixture();
    schema["properties"]["journal"]["x-ck-setup"]["label"] = json!("Not a section title");
    refused(schema, "/journal");
}

#[test]
fn setup_refuses_invalid_pointer() {
    for target in [
        "journal/enabled",
        "#/journal/enabled",
        "/journal/~2",
        "/journal/~",
    ] {
        refused(recommended(target, json!(true)), "/journal");
    }
}

#[test]
fn setup_refuses_pointer_outside_section() {
    for target in ["", "/archive/enabled", "/journal_extra/enabled"] {
        refused(recommended(target, json!(true)), "/journal");
    }
}

#[test]
fn setup_refuses_pointer_to_no_field() {
    refused(recommended("/journal/missing", json!(true)), "/journal");
}

#[test]
fn setup_refuses_value_rejected_by_field_schema() {
    for (target, value) in [
        ("/journal/enabled", json!("true")),
        ("/journal/nested/mode", json!("unknown")),
    ] {
        refused(recommended(target, value), "/journal");
    }
}

fn credential() -> Value {
    let mut schema = field("object", json!({"vault_ref":"example"}));
    schema["x-ck-kind"] = json!("credential_ref");
    schema
}

#[test]
fn setup_refuses_credential_target() {
    let mut schema = recommended("/journal/token", json!({"vault_ref":"example"}));
    schema["properties"]["journal"]["properties"]["token"] = credential();
    refused(schema, "/journal");
}

#[test]
fn setup_refuses_array_containing_credentials_even_when_empty() {
    let mut schema = recommended("/journal/accounts", json!([]));
    let mut accounts = field("array", json!([]));
    accounts["items"] = object(json!({"token":credential()}));
    schema["properties"]["journal"]["properties"]["accounts"] = accounts;
    refused(schema, "/journal");
}

#[test]
fn setup_refuses_non_object_property() {
    let mut schema = fixture();
    let setup = schema["properties"]["journal"]
        .as_object_mut()
        .unwrap()
        .remove("x-ck-setup")
        .unwrap();
    let mut flag = field("boolean", json!(false));
    flag["x-ck-setup"] = setup;
    schema["properties"]["flag"] = flag;
    refused(schema, "/flag");
}

#[test]
fn setup_refuses_root_annotation() {
    let mut schema = fixture();
    schema["x-ck-setup"] = json!({"feature":"root","order":0,"recommended":{}});
    refused(schema, "");
}

#[test]
fn setup_refuses_nested_section() {
    let mut schema = fixture();
    schema["properties"]["journal"]["properties"]["nested"]["x-ck-setup"] =
        json!({"feature":"nested","order":0,"recommended":{}});
    refused(schema, "/journal/nested");
}

#[test]
fn setup_refuses_object_target() {
    for target in ["/journal", "/journal/nested"] {
        refused(recommended(target, json!({"mode":"brief"})), "/journal");
    }
}

#[test]
fn setup_refuses_array_item_target() {
    let mut schema = recommended("/journal/tags/0", json!("one"));
    let mut tags = field("array", json!([]));
    tags["items"] = field("string", json!(""));
    schema["properties"]["journal"]["properties"]["tags"] = tags;
    refused(schema, "/journal");
}

#[test]
fn setup_refuses_pattern_property_target() {
    let mut schema = recommended("/journal/labels/one", json!(true));
    let mut labels = object(json!({}));
    labels["patternProperties"] = json!({"^[a-z]+$":field("boolean", json!(false))});
    schema["properties"]["journal"]["properties"]["labels"] = labels;
    refused(schema, "/journal");
}

#[test]
fn setup_refuses_unresolved_union_target() {
    let mut schema = recommended("/journal/choice/enabled", json!(true));
    let branch = |name| {
        let mut tag = field("string", json!(name));
        tag["const"] = json!(name);
        let mut branch = object(json!({"kind":tag,"enabled":field("boolean",json!(false))}));
        branch["required"] = json!(["kind"]);
        branch
    };
    schema["properties"]["journal"]["properties"]["choice"] =
        json!({"oneOf":[branch("one"),branch("two")]});
    refused(schema, "/journal");
}
