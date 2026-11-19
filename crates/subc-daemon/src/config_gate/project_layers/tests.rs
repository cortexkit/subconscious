use super::*;
use serde_json::json;
use subc_protocol::config::ConfigStatus;

fn schema() -> ProjectLayerSchema {
    ProjectLayerSchema::new(Schema::new(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "unevaluatedProperties":false,
        "x-ck-layers":["user","workspace","project"], "x-ck-apply":"live",
        "$defs": {"credential":{"type":"object", "description":"Vault reference", "x-ck-required":true, "x-ck-kind":"credential_ref", "x-ck-layers":["user"]}},
        "properties": {
            "s":{"type":"object", "unevaluatedProperties":false, "properties":{
                "nested":{"type":"object", "unevaluatedProperties":false, "properties":{"credential":{"$ref":"#/$defs/credential"}}},
                "label":{"type":"string", "description":"Label", "default":"default"}
            }},
            "env-public":{"type":"string", "description":"Ordinary exact property", "default":"ordinary"},
            "next":{"type":"integer", "description":"Next supervision", "default":0, "x-ck-apply":"next_supervision"},
            "route":{"type":"integer", "description":"Route rebind", "default":0, "x-ck-apply":"route_rebind"},
            "limit":{"type":"integer", "description":"Limit", "default":0}
        },
        "patternProperties":{"^env-":{"type":"object", "unevaluatedProperties":false, "properties":{
            "label":{"type":"string", "description":"Label", "default":"default"},
            "credential":{"$ref":"#/$defs/credential"}
        }}}
    })).unwrap())
}

#[test]
fn project_layers_refuse_expansions_without_echoing_literals_or_keys() {
    let schema = schema();
    for layer in [Layer::Workspace, Layer::Project] {
        for literal in ["{env:EXPANSION_SECRET}", "{file:EXPANSION_SECRET}"] {
            let input = json!({literal: [literal]});
            let error = schema
                .record_layer(
                    layer,
                    &serde_json::to_vec(&input).unwrap(),
                    "module",
                    "project",
                )
                .err()
                .unwrap();
            assert_eq!(error.code, "value_invalid");
            assert!(!error.to_string().contains("EXPANSION_SECRET"));
        }
    }
}

#[test]
fn project_layers_restrict_named_and_pattern_credential_sections_before_review() {
    let schema = schema();
    for layer in [Layer::Workspace, Layer::Project] {
        for input in [
            br#"{"s":{"label":"sibling"}}"#.as_slice(),
            br#"{"s":{"nested":{"credential":{"vault_ref":"private-reference"}}}}"#,
            br#"{"env-prod":{"label":"dynamic sibling"}}"#,
        ] {
            let error = schema
                .record_layer(layer, input, "module", "p")
                .err()
                .unwrap();
            assert_eq!(error.code, "credential_layer_unsupported");
            assert!(!error.detail.contains("private-reference"));
        }
        let allowed = schema
            .record_layer(
                layer,
                br#"{ /* comment */ "limit":5, "env-public":"allowed", }"#,
                "module",
                "p",
            )
            .unwrap();
        assert_eq!(allowed.recorded, json!({"limit":5,"env-public":"allowed"}));
        let user = json!({"s":{"nested":{"credential":{"vault_ref":"user-reference"}}}});
        let reviewed = schema
            .review_tuple("module", "p", Some(user), &[allowed], None)
            .unwrap();
        assert_eq!(reviewed.effective["limit"], 5);
        assert_eq!(reviewed.sources["/limit"], layer);
    }
}

#[test]
fn project_layers_refuse_non_live_base_or_candidate_sources_but_allow_user_sources() {
    let schema = schema();
    let user =
        json!({"next":1,"route":1,"s":{"nested":{"credential":{"vault_ref":"user-reference"}}}});
    let inherited = schema
        .review_tuple("module", "p", Some(user.clone()), &[], None)
        .unwrap();
    for (field, layer) in [("next", Layer::Workspace), ("route", Layer::Project)] {
        let bytes = serde_json::to_vec(&json!({field: 2})).unwrap();
        let layer_value = schema.record_layer(layer, &bytes, "module", "p").unwrap();
        let error = schema
            .review_tuple("module", "p", Some(user.clone()), &[layer_value], None)
            .unwrap_err();
        assert_eq!(error.code, "project_apply_kind_unsupported");
        assert!(error.detail.contains(&format!("/{field}")));
        let base = ConfigView::new(
            1,
            json!({field: 2}),
            BTreeMap::from([(format!("/{field}"), layer)]),
            vec![],
            ConfigStatus::Active,
        );
        let error = refuse_project_apply_kinds(
            "module",
            "p",
            Some((schema.schema(), &base)),
            schema.schema(),
            &inherited,
        )
        .unwrap_err();
        assert_eq!(error.code, "project_apply_kind_unsupported");
        assert!(error.detail.contains(&format!("/{field}")));
    }
}
