//! Daemon-side validation of a module's pure immutable review reply.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use subc_config::{ApplyKind, FieldChange, Layer, RecordedValue, Schema};
use subc_protocol::config::{ConfigReviewReply, ConfigView};

use super::{GateError, Result};

pub(crate) fn checked_fields(
    module: &str,
    schema: &Schema,
    base: Option<&ConfigView>,
    base_schema: Option<&Schema>,
    candidate: &ConfigView,
    reply: ConfigReviewReply,
) -> Result<Vec<FieldChange>> {
    if let Some(refusal) = reply.refusals.first() {
        // Refusal prose is module-authored. Only bounded field pointers leave
        // this boundary, never unvalidated values echoed by a reviewer.
        let paths: Vec<_> = refusal
            .paths
            .iter()
            .filter(|p| p.starts_with('/') && p.len() <= 1024)
            .collect();
        return Err(GateError::new(
            "module_review_refused",
            format!("{module}: {paths:?}"),
        ));
    }
    let mut paths = BTreeSet::new();
    leaf_paths(&candidate.effective, "", &mut paths);
    if let Some(base) = base {
        leaf_paths(&base.effective, "", &mut paths);
    }
    let old_recorded = if let Some(base) = base {
        Some(
            base_schema
                .ok_or_else(|| {
                    GateError::new(
                        "approved_schema_unavailable",
                        format!("{module}: approved old schema is missing or damaged"),
                    )
                })?
                .record(&base.effective)
                .map_err(GateError::invalid)?,
        )
    } else {
        None
    };
    let new_recorded = schema
        .record(&candidate.effective)
        .map_err(GateError::invalid)?;
    let mut expected = BTreeMap::new();
    for path in paths {
        let old = base.and_then(|b| b.effective.pointer(&path));
        let new = candidate.effective.pointer(&path);
        let old_source = base
            .and_then(|b| source(&b.sources, &path))
            .unwrap_or(Layer::Default);
        let new_source = source(&candidate.sources, &path).unwrap_or(Layer::Default);
        if old == new && old_source == new_source {
            continue;
        }
        let mut field = FieldChange::new(
            module,
            &path,
            recorded_leaf(old_recorded.as_ref().and_then(|v| v.pointer(&path))),
            recorded_leaf(new_recorded.pointer(&path)),
            if new.is_none() {
                let old_view = base.ok_or_else(|| invalid(module))?;
                apply_for(
                    base_schema
                        .ok_or_else(|| {
                            GateError::new(
                                "approved_schema_unavailable",
                                format!("{module}: removed field has no approved old schema"),
                            )
                        })?
                        .normalized(),
                    &old_view.effective,
                    &path,
                )?
            } else {
                apply_for(schema.normalized(), &candidate.effective, &path)?
            },
        );
        field.project_id = candidate.project_id.clone();
        field.old_source = old_source;
        field.new_source = new_source;
        expected.insert(path, field);
    }
    let mut seen = BTreeSet::new();
    for mut field in reply.fields {
        let Some(wanted) = expected.get(&field.path) else {
            return Err(invalid(module));
        };
        if !seen.insert(field.path.clone()) {
            return Err(invalid(module));
        }
        // Reasons may add explanation, but cannot alter the checked old/new
        // values, source layers, apply kind, module or project identity.
        field.reasons.clear();
        if field != *wanted {
            return Err(invalid(module));
        }
    }
    if seen.len() != expected.len() {
        return Err(invalid(module));
    }
    Ok(expected.into_values().collect())
}

fn invalid(module: &str) -> GateError {
    GateError::new(
        "module_review_invalid",
        format!("{module}: reply differs from the schema-validated immutable old/new view"),
    )
}

fn recorded_leaf(value: Option<&Value>) -> RecordedValue {
    let value = value.cloned().unwrap_or(Value::Null);
    if value.get("kind").and_then(Value::as_str) == Some("redacted") {
        if let Ok(record) = serde_json::from_value(value.clone()) {
            return record;
        }
    }
    RecordedValue::new(value)
}

fn source(sources: &BTreeMap<String, Layer>, path: &str) -> Option<Layer> {
    if let Some((_, layer)) = sources
        .iter()
        .find(|(p, _)| p.starts_with(&format!("{path}/")))
    {
        return Some(*layer);
    }
    let mut path = path;
    loop {
        if let Some(layer) = sources.get(path) {
            return Some(*layer);
        }
        path = path.rsplit_once('/')?.0;
    }
}

fn leaf_paths(value: &Value, path: &str, paths: &mut BTreeSet<String>) {
    if let Value::Object(object) = value {
        if !object.is_empty() && !(object.len() == 1 && object.contains_key("vault_ref")) {
            for (key, value) in object {
                leaf_paths(
                    value,
                    &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                    paths,
                );
            }
            return;
        }
    }
    if !path.is_empty() {
        paths.insert(path.into());
    }
}

fn apply_for(schema: &Value, value: &Value, path: &str) -> Result<ApplyKind> {
    let mut schema = schema;
    let mut value = Some(value);
    let mut apply = schema.get("x-ck-apply");
    for part in path.split('/').skip(1) {
        let key = part.replace("~1", "/").replace("~0", "~");
        let branch = schema
            .get("oneOf")
            .and_then(Value::as_array)
            .and_then(|branches| {
                branches.iter().find(|branch| {
                    let Some(properties) = branch.get("properties").and_then(Value::as_object)
                    else {
                        return false;
                    };
                    properties.iter().any(|(key, property)| {
                        property
                            .get("const")
                            .is_some_and(|tag| value.and_then(|value| value.get(key)) == Some(tag))
                    }) && properties.iter().all(|(key, property)| {
                        property.get("const").is_none_or(|tag| {
                            value
                                .and_then(|value| value.get(key))
                                .is_none_or(|value| value == tag)
                        })
                    })
                })
            })
            .unwrap_or(schema);
        apply = branch.get("x-ck-apply").or(apply);
        schema = branch
            .get("properties")
            .and_then(|p| p.get(&key))
            .or_else(|| schema.get("properties").and_then(|p| p.get(&key)))
            .or_else(|| schema.get("additionalProperties").filter(|v| v.is_object()))
            .or_else(|| schema.get("items"))
            .ok_or_else(|| {
                GateError::new("module_review_invalid", "field has no declared schema")
            })?;
        value = value.and_then(|value| value.get(&key));
        apply = schema.get("x-ck-apply").or(apply);
    }
    serde_json::from_value(
        apply
            .cloned()
            .unwrap_or_else(|| Value::String("live".into())),
    )
    .map_err(|_| GateError::new("schema_invalid", "field has an invalid apply kind"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use subc_protocol::config::ConfigStatus;

    #[test]
    fn config_review_checks_immutable_old_values_sources_and_apply() {
        let schema = Schema::new(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"module_restart","properties":{"limit":{"type":"integer","description":"Limit","default":1}}})).unwrap();
        let base = ConfigView::new(
            1,
            json!({"limit":1}),
            BTreeMap::from([("/limit".into(), Layer::User)]),
            vec![],
            ConfigStatus::Active,
        );
        let candidate = ConfigView::new(
            2,
            json!({"limit":2}),
            BTreeMap::from([("/limit".into(), Layer::User)]),
            vec![],
            ConfigStatus::Pending,
        );
        let mut field = FieldChange::new(
            "worker",
            "/limit",
            RecordedValue::new(json!(1)),
            RecordedValue::new(json!(2)),
            ApplyKind::ModuleRestart,
        );
        field.old_source = Layer::User;
        field.new_source = Layer::User;
        assert!(checked_fields(
            "worker",
            &schema,
            Some(&base),
            Some(&schema),
            &candidate,
            ConfigReviewReply::new(vec![field.clone()], vec![])
        )
        .is_ok());
        field.old = RecordedValue::new(json!(0));
        assert_eq!(
            checked_fields(
                "worker",
                &schema,
                Some(&base),
                Some(&schema),
                &candidate,
                ConfigReviewReply::new(vec![field], vec![])
            )
            .unwrap_err()
            .code,
            "module_review_invalid"
        );
        assert!(checked_fields(
            "worker",
            &schema,
            Some(&base),
            Some(&schema),
            &candidate,
            ConfigReviewReply::new(vec![], vec![])
        )
        .is_err());
    }

    #[test]
    fn config_review_keeps_old_credential_redaction_when_field_becomes_ordinary() {
        let old_schema = Schema::new(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"live","properties":{"credential":{"type":"object","description":"Credential","x-ck-kind":"credential_ref","x-ck-required":true}}})).unwrap();
        let schema = Schema::new(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"live","properties":{"credential":{"type":"object","description":"Ordinary value","x-ck-required":true,"unevaluatedProperties":false,"properties":{"vault_ref":{"type":"string","description":"Ordinary identifier","x-ck-required":true}}}}})).unwrap();
        let reference = json!({"vault_ref":"old-reference-must-stay-hidden"});
        let base = ConfigView::new(
            1,
            json!({"credential":reference}),
            BTreeMap::from([("/credential".into(), Layer::User)]),
            vec![],
            ConfigStatus::Active,
        );
        let candidate = ConfigView::new(
            2,
            json!({"credential":{"vault_ref":"ordinary-data"}}),
            BTreeMap::from([("/credential".into(), Layer::User)]),
            vec![],
            ConfigStatus::Pending,
        );
        let mut field = FieldChange::new(
            "worker",
            "/credential",
            RecordedValue::redacted(&reference).unwrap(),
            RecordedValue::new(json!({"vault_ref":"ordinary-data"})),
            ApplyKind::Live,
        );
        field.old_source = Layer::User;
        field.new_source = Layer::User;
        let fields = checked_fields(
            "worker",
            &schema,
            Some(&base),
            Some(&old_schema),
            &candidate,
            ConfigReviewReply::new(vec![field.clone()], vec![]),
        )
        .unwrap();
        let mut changeset = subc_config::Changeset::new();
        changeset.fields = fields;
        let plan = subc_config::Plan::new(
            1,
            1,
            "review",
            subc_config::InputVector::default(),
            changeset,
        );
        assert!(!serde_json::to_string(&plan)
            .unwrap()
            .contains("old-reference-must-stay-hidden"));
        assert_eq!(
            checked_fields(
                "worker",
                &schema,
                Some(&base),
                None,
                &candidate,
                ConfigReviewReply::new(vec![field], vec![])
            )
            .unwrap_err()
            .code,
            "approved_schema_unavailable"
        );
    }

    #[test]
    fn config_review_removed_restart_field_keeps_its_approved_apply_kind() {
        let old_schema = Schema::new(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"x-ck-layers":["user"],"properties":{"old_limit":{"type":"integer","description":"Old limit","x-ck-apply":"module_restart","default":3}}})).unwrap();
        let schema = Schema::new(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"x-ck-layers":["user"],"properties":{}})).unwrap();
        let base = ConfigView::new(
            1,
            json!({"old_limit":3}),
            BTreeMap::from([("/old_limit".into(), Layer::User)]),
            vec![],
            ConfigStatus::Active,
        );
        let candidate =
            ConfigView::new(2, json!({}), BTreeMap::new(), vec![], ConfigStatus::Pending);
        let mut field = FieldChange::new(
            "worker",
            "/old_limit",
            RecordedValue::new(json!(3)),
            RecordedValue::new(Value::Null),
            ApplyKind::ModuleRestart,
        );
        field.old_source = Layer::User;
        let fields = checked_fields(
            "worker",
            &schema,
            Some(&base),
            Some(&old_schema),
            &candidate,
            ConfigReviewReply::new(vec![field], vec![]),
        )
        .unwrap();
        assert_eq!(fields[0].apply, ApplyKind::ModuleRestart);
    }

    #[test]
    fn config_review_resolves_apply_policy_from_the_effective_union_branch() {
        let schema=Schema::new(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"live","properties":{"storage":{"default":{"kind":"disk","path":"a"},"oneOf":[
            {"type":"object","unevaluatedProperties":false,"required":["kind","path"],"properties":{"kind":{"type":"string","description":"Storage kind","const":"disk","default":"disk"},"path":{"type":"string","description":"Disk path","default":"a","x-ck-apply":"module_restart"}}},
            {"type":"object","unevaluatedProperties":false,"required":["kind","capacity"],"properties":{"kind":{"type":"string","description":"Storage kind","const":"memory","default":"memory"},"capacity":{"type":"integer","description":"Memory capacity","default":1}}}
        ]}}})).unwrap();
        let sources = BTreeMap::from([("/storage/path".into(), Layer::User)]);
        let base = ConfigView::new(
            1,
            json!({"storage":{"kind":"disk","path":"a"}}),
            sources.clone(),
            vec![],
            ConfigStatus::Active,
        );
        let candidate = ConfigView::new(
            2,
            json!({"storage":{"kind":"disk","path":"b"}}),
            sources,
            vec![],
            ConfigStatus::Pending,
        );
        let mut field = FieldChange::new(
            "worker",
            "/storage/path",
            RecordedValue::new(json!("a")),
            RecordedValue::new(json!("b")),
            ApplyKind::ModuleRestart,
        );
        field.old_source = Layer::User;
        field.new_source = Layer::User;
        assert!(checked_fields(
            "worker",
            &schema,
            Some(&base),
            Some(&schema),
            &candidate,
            ConfigReviewReply::new(vec![field], vec![])
        )
        .is_ok());
        assert_eq!(
            apply_for(
                schema.normalized(),
                &json!({"storage":{"kind":"memory","capacity":2}}),
                "/storage/capacity"
            )
            .unwrap(),
            ApplyKind::Live
        );
    }
}
