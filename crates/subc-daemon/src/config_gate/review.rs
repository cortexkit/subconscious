//! Daemon-side validation of a module's pure immutable review reply.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use subc_config::{ApplyKind, FieldChange, Layer, RecordedValue, Schema};
use subc_protocol::config::{ConfigLayer, ConfigReviewReply, ConfigView};

use super::{GateError, Result};

pub(crate) fn checked_fields(
    module: &str,
    schema: &Schema,
    base: Option<&ConfigView>,
    base_schema: Option<&Schema>,
    candidate: &ConfigView,
    reply: ConfigReviewReply,
) -> Result<Vec<FieldChange>> {
    if !reply.refusals.is_empty() {
        // Refusal prose is module-authored. Only bounded field pointers leave
        // this boundary, never unvalidated values echoed by a reviewer.
        let paths: BTreeSet<_> = reply
            .refusals
            .iter()
            .flat_map(|refusal| &refusal.paths)
            .filter(|p| p.starts_with('/') && p.len() <= 1024)
            .collect();
        let attribution = attributed_review_refusals(base, candidate, &reply.refusals);
        return Err(
            GateError::new("module_review_refused", format!("{module}: {paths:?}"))
                .with_settings_review_paths(attribution),
        );
    }
    let expected = expected_fields(module, schema, base, base_schema, candidate)?;
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

pub(crate) fn attributed_review_refusals(
    base: Option<&ConfigView>,
    candidate: &ConfigView,
    refusals: &[subc_protocol::config::ConfigReviewRefusal],
) -> Vec<std::path::PathBuf> {
    if refusals.is_empty() {
        return Vec::new();
    }
    if candidate.project_id.is_some() {
        let pointers = refusals
            .iter()
            .flat_map(|refusal| refusal.paths.iter().map(String::as_str));
        project_layer_changes(
            base,
            candidate,
            pointers,
            refusals.iter().any(|refusal| refusal.paths.is_empty()),
        )
    } else {
        candidate
            .layers
            .iter()
            .filter(|layer| layer.layer == ConfigLayer::User)
            .filter_map(|layer| layer.path.as_ref().map(std::path::PathBuf::from))
            .collect()
    }
}

pub(crate) fn changed_project_review_paths(
    base: Option<&ConfigView>,
    candidate: &ConfigView,
) -> Vec<std::path::PathBuf> {
    project_layer_changes(base, candidate, std::iter::empty(), true)
}

fn project_layer_changes<'a>(
    base: Option<&ConfigView>,
    candidate: &ConfigView,
    pointers: impl IntoIterator<Item = &'a str>,
    all_changed: bool,
) -> Vec<std::path::PathBuf> {
    let documents = |view: Option<&ConfigView>| -> BTreeMap<_, _> {
        view.into_iter()
            .flat_map(|view| &view.layers)
            .filter_map(|layer| {
                let path = std::path::PathBuf::from(layer.path.as_ref()?);
                let rank = match layer.layer {
                    ConfigLayer::User => Some(1),
                    ConfigLayer::Workspace => Some(2),
                    ConfigLayer::Project => Some(3),
                    _ => None,
                };
                let document = layer
                    .bytes
                    .as_deref()
                    .map(super::capture::parse_layer)
                    .transpose();
                Some((path, (rank, document)))
            })
            .collect()
    };
    let before = documents(base);
    let after = documents(Some(candidate));
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let mut refused = BTreeSet::new();
    // An unscoped refusal belongs to the user file. For a project review,
    // compare each physical layer at the refused pointer, including a missing
    // candidate document for a deletion. Prefer the narrower changed layer.
    // If the comparison cannot choose a rank, keep every changed contributor
    // at that pointer, but never attribute a refusal to an unchanged layer.
    // A refusal row without pointers names the entire review, so it does not
    // have enough information to choose a narrower layer.
    if all_changed {
        refused.extend(
            paths
                .iter()
                .filter(|path| {
                    let old = before.get(**path).map(|(_, doc)| doc);
                    let new = after.get(**path).map(|(_, doc)| doc);
                    if old.is_some_and(Result::is_err) || new.is_some_and(Result::is_err) {
                        return false;
                    }
                    old.and_then(|doc| doc.as_ref().ok()?.as_ref())
                        != new.and_then(|doc| doc.as_ref().ok()?.as_ref())
                })
                .map(|path| (*path).clone()),
        );
    }
    for pointer in pointers
        .into_iter()
        .filter(|pointer| pointer.len() <= 1024 && (pointer.is_empty() || pointer.starts_with('/')))
    {
        let changed: Vec<_> = paths
            .iter()
            .filter_map(|path| {
                let old = before.get(*path);
                let new = after.get(*path);
                if old.is_some_and(|(_, document)| document.is_err())
                    || new.is_some_and(|(_, document)| document.is_err())
                {
                    return None;
                }
                let old_value = old
                    .and_then(|(_, document)| document.as_ref().ok()?.as_ref())
                    .and_then(|value| value.pointer(pointer));
                let new_value = new
                    .and_then(|(_, document)| document.as_ref().ok()?.as_ref())
                    .and_then(|value| value.pointer(pointer));
                (old_value != new_value).then(|| (*path, new.or(old).and_then(|(rank, _)| *rank)))
            })
            .collect();
        let narrower = if changed.iter().all(|(_, rank)| rank.is_some()) {
            changed.iter().filter_map(|(_, rank)| *rank).max()
        } else {
            None
        };
        refused.extend(
            changed
                .into_iter()
                .filter(|(_, rank)| narrower.is_none_or(|narrower| *rank == Some(narrower)))
                .map(|(path, _)| path.clone()),
        );
    }
    refused.into_iter().collect()
}

pub(super) fn expected_fields(
    module: &str,
    schema: &Schema,
    base: Option<&ConfigView>,
    base_schema: Option<&Schema>,
    candidate: &ConfigView,
) -> Result<BTreeMap<String, FieldChange>> {
    let new_leaves = leaves(schema, &candidate.effective);
    let (old_leaves, old_recorded) = if let Some(base) = base {
        let old_schema = base_schema.ok_or_else(|| {
            GateError::new(
                "approved_schema_unavailable",
                format!("{module}: approved old schema is missing or damaged"),
            )
        })?;
        (
            leaves(old_schema, &base.effective),
            Some(
                old_schema
                    .record(&base.effective)
                    .map_err(GateError::invalid)?,
            ),
        )
    } else {
        (BTreeMap::new(), None)
    };
    let paths: BTreeSet<_> = old_leaves.keys().chain(new_leaves.keys()).collect();
    let new_recorded = schema
        .record(&candidate.effective)
        .map_err(GateError::invalid)?;
    let mut expected = BTreeMap::new();
    let apply = if candidate.project_id.is_some() {
        apply_for_project
    } else {
        apply_for
    };
    for path in paths {
        let old_opaque = opaque_at(&old_leaves, path);
        let new_opaque = opaque_at(&new_leaves, path);
        let old_hidden = hidden_by_opacity(&old_leaves, path, new_opaque.is_some());
        let new_hidden = hidden_by_opacity(&new_leaves, path, old_opaque.is_some());
        let old = base
            .filter(|_| !old_hidden)
            .and_then(|b| b.effective.pointer(path));
        let new = (!new_hidden)
            .then(|| candidate.effective.pointer(path))
            .flatten();
        let old_source = base
            .filter(|_| !old_hidden)
            .and_then(|b| source(&b.sources, path))
            .unwrap_or(Layer::Default);
        let new_source = (!new_hidden)
            .then(|| source(&candidate.sources, path))
            .flatten()
            .unwrap_or(Layer::Default);
        if old == new && old_source == new_source {
            continue;
        }
        let mut field = FieldChange::new(
            module,
            path,
            recorded_leaf(
                old.and_then(|_| old_recorded.as_ref()?.pointer(path)),
                old_opaque.is_some(),
            ),
            recorded_leaf(
                new.and_then(|_| new_recorded.pointer(path)),
                new_opaque.is_some(),
            ),
            if new.is_none() {
                let old_view = base.ok_or_else(|| invalid(module))?;
                if let Some(schema) = old_opaque {
                    apply_kind(schema.get("x-ck-apply"))?
                } else {
                    apply(
                        base_schema
                            .ok_or_else(|| {
                                GateError::new(
                                    "approved_schema_unavailable",
                                    format!("{module}: removed field has no approved old schema"),
                                )
                            })?
                            .normalized(),
                        &old_view.effective,
                        path,
                    )?
                }
            } else if let Some(schema) = new_opaque {
                apply_kind(schema.get("x-ck-apply"))?
            } else {
                apply(schema.normalized(), &candidate.effective, path)?
            },
        );
        field.project_id = candidate.project_id.clone();
        field.old_source = old_source;
        field.new_source = new_source;
        expected.insert(path.clone(), field);
    }
    Ok(expected)
}

fn invalid(module: &str) -> GateError {
    GateError::new(
        "module_review_invalid",
        format!("{module}: reply differs from the schema-validated immutable old/new view"),
    )
}

fn recorded_leaf(value: Option<&Value>, opaque: bool) -> RecordedValue {
    let value = value.cloned().unwrap_or(Value::Null);
    if !opaque && value.get("kind").and_then(Value::as_str) == Some("redacted") {
        if let Ok(record) = serde_json::from_value(value.clone()) {
            return record;
        }
    }
    RecordedValue::new(value)
}

pub(super) fn source(sources: &BTreeMap<String, Layer>, path: &str) -> Option<Layer> {
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

#[derive(Clone, Copy)]
enum Leaf<'a> {
    Regular,
    Section,
    Opaque(&'a Value),
}

fn opaque_at<'a>(leaves: &BTreeMap<String, Leaf<'a>>, path: &str) -> Option<&'a Value> {
    match leaves.get(path) {
        Some(Leaf::Opaque(schema)) => Some(schema),
        _ => None,
    }
}

fn hidden_by_opacity(
    leaves: &BTreeMap<String, Leaf<'_>>,
    mut path: &str,
    other_opaque: bool,
) -> bool {
    if other_opaque && matches!(leaves.get(path), None | Some(Leaf::Section)) {
        return true;
    }
    // An old or new JSON object may contain the same pointer as a managed
    // field on the other side of a schema migration. It is not a field here.
    while let Some((parent, _)) = path.rsplit_once('/') {
        if opaque_at(leaves, parent).is_some() {
            return true;
        }
        path = parent;
    }
    false
}

pub(super) fn leaf_paths<'a>(
    schema: &'a Schema,
    value: &Value,
) -> impl Iterator<Item = String> + 'a {
    leaves(schema, value).into_keys()
}

fn leaves<'a>(schema: &'a Schema, value: &Value) -> BTreeMap<String, Leaf<'a>> {
    let mut leaves = BTreeMap::new();
    visit_leaves(schema.normalized(), value, "", &mut leaves);
    leaves
}

fn visit_leaves<'a>(
    schema: &'a Value,
    value: &Value,
    path: &str,
    leaves: &mut BTreeMap<String, Leaf<'a>>,
) {
    if schema.get("x-ck-kind").and_then(Value::as_str) == Some("json") {
        leaves.insert(path.into(), Leaf::Opaque(schema));
        return;
    }
    if let Value::Object(object) = value {
        let branch = select_branch(schema, Some(value));
        let vault = object.len() == 1 && object.contains_key("vault_ref");
        if !object.is_empty()
            && (!vault
                || field_schema(schema, branch, "vault_ref").is_some_and(|child| {
                    child.get("x-ck-kind").and_then(Value::as_str) == Some("json")
                }))
        {
            for (key, value) in object {
                visit_leaves(
                    field_schema(schema, branch, key).unwrap_or(&Value::Null),
                    value,
                    &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                    leaves,
                );
            }
            return;
        }
    }
    if !path.is_empty() {
        let leaf = if value.is_object()
            && schema.get("x-ck-kind").and_then(Value::as_str) != Some("credential_ref")
        {
            Leaf::Section
        } else {
            Leaf::Regular
        };
        leaves.insert(path.into(), leaf);
    }
}

fn apply_for(schema: &Value, value: &Value, path: &str) -> Result<ApplyKind> {
    apply_for_path(schema, value, path)
}

pub(super) fn apply_for_project(schema: &Value, value: &Value, path: &str) -> Result<ApplyKind> {
    apply_for_path(schema, value, path)
}

/// The apply kind for the field at `path`, walking declared properties and the
/// structural maps' `patternProperties` the same way validation does, so a
/// map's pattern-keyed entries are reviewable like named fields.
fn apply_for_path(schema: &Value, value: &Value, path: &str) -> Result<ApplyKind> {
    let mut schema = schema;
    let mut value = Some(value);
    let mut apply = schema.get("x-ck-apply");
    for part in path.split('/').skip(1) {
        let key = part.replace("~1", "/").replace("~0", "~");
        let branch = select_branch(schema, value);
        apply = branch.get("x-ck-apply").or(apply);
        schema = field_schema(schema, branch, &key).ok_or_else(|| {
            GateError::new("module_review_invalid", "field has no declared schema")
        })?;
        value = value.and_then(|value| value.get(&key));
        apply = schema.get("x-ck-apply").or(apply);
    }
    apply_kind(apply)
}

fn select_branch<'a>(schema: &'a Value, value: Option<&Value>) -> &'a Value {
    schema
        .get("oneOf")
        .and_then(Value::as_array)
        .and_then(|branches| {
            branches.iter().find(|branch| {
                let Some(properties) = branch.get("properties").and_then(Value::as_object) else {
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
        .unwrap_or(schema)
}

fn field_schema<'a>(schema: &'a Value, branch: &'a Value, key: &str) -> Option<&'a Value> {
    branch
        .get("properties")
        .and_then(|p| p.get(key))
        .or_else(|| schema.get("properties").and_then(|p| p.get(key)))
        .or_else(|| {
            branch
                .get("patternProperties")
                .and_then(Value::as_object)?
                .iter()
                .find(|(pattern, _)| {
                    regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(key))
                })
                .map(|(_, schema)| schema)
        })
        .or_else(|| schema.get("additionalProperties").filter(|v| v.is_object()))
        .or_else(|| schema.get("items"))
}

fn apply_kind(apply: Option<&Value>) -> Result<ApplyKind> {
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

    fn settings_schema(opaque: bool) -> Schema {
        let setting = if opaque {
            json!({"x-ck-kind":"json","x-ck-layers":["user"],"x-ck-apply":"module_restart","description":"A settings object","default":{}})
        } else {
            json!({"type":"object","unevaluatedProperties":false,"properties":{
                "a":{"type":"number","description":"A number","default":0,"x-ck-apply":"live","x-ck-layers":["user"]}
            }})
        };
        Schema::new(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"properties":{"setting":setting}})).unwrap()
    }

    fn settings_view(schema: &Schema, value: Value) -> ConfigView {
        let tuple = schema
            .review_tuple(
                &[subc_config::LayerValue::new(
                    Layer::User,
                    json!({"setting":value}),
                )],
                &subc_config::ValidationContext::new("worker", None),
            )
            .unwrap();
        ConfigView::new(
            1,
            tuple.effective,
            tuple.sources,
            vec![],
            ConfigStatus::Active,
        )
    }

    #[test]
    fn config_pending_settings_review_attributes_every_refusal_and_empty_pointer_list() {
        use std::path::PathBuf;
        use subc_protocol::config::{ConfigLayerSnapshot, ConfigReviewRefusal};

        let schema = settings_schema(false);
        let view = |user: &[u8], workspace: Option<&[u8]>| {
            let mut view = settings_view(&schema, json!({"a": 1}));
            view.project_id = Some("project".into());
            view.layers = vec![
                ConfigLayerSnapshot::file(
                    "worker",
                    ConfigLayer::User,
                    "user.jsonc",
                    Some(user.to_vec()),
                ),
                ConfigLayerSnapshot::file(
                    "worker",
                    ConfigLayer::Workspace,
                    "workspace.jsonc",
                    workspace.map(<[u8]>::to_vec),
                ),
                ConfigLayerSnapshot::file(
                    "worker",
                    ConfigLayer::Project,
                    "unchanged.jsonc",
                    Some(b"{}".to_vec()),
                ),
            ];
            view
        };
        let base = view(br#"{"common":1,"onlyUser":1}"#, Some(br#"{"common":2}"#));
        let candidate = view(br#"{"common":3,"onlyUser":2}"#, None);
        let refuse = |pointers: &[&str]| {
            ConfigReviewRefusal::new(
                "refused",
                pointers.iter().map(|p| (*p).into()).collect(),
                vec![],
            )
        };
        for (refusals, expected) in [
            (
                vec![refuse(&["/common"])],
                vec![PathBuf::from("workspace.jsonc")],
            ),
            (
                vec![refuse(&["/common"]), refuse(&["/onlyUser"])],
                vec![
                    PathBuf::from("user.jsonc"),
                    PathBuf::from("workspace.jsonc"),
                ],
            ),
            (
                vec![refuse(&[])],
                vec![
                    PathBuf::from("user.jsonc"),
                    PathBuf::from("workspace.jsonc"),
                ],
            ),
        ] {
            let error = checked_fields(
                "worker",
                &schema,
                Some(&base),
                Some(&schema),
                &candidate,
                ConfigReviewReply::new(vec![], refusals),
            )
            .unwrap_err();
            assert_eq!(error.code, "module_review_refused");
            assert_eq!(error.settings_review_paths, expected);
        }
        let mut unscoped = candidate;
        unscoped.project_id = None;
        let error = checked_fields(
            "worker",
            &schema,
            Some(&base),
            Some(&schema),
            &unscoped,
            ConfigReviewReply::new(vec![], vec![refuse(&["/unchanged"])]),
        )
        .unwrap_err();
        assert_eq!(error.settings_review_paths, [PathBuf::from("user.jsonc")]);
    }

    #[test]
    fn config_review_reports_opaque_json_as_one_whole_field() {
        let schema = settings_schema(true);
        let old = json!(RecordedValue::redacted(&json!("ordinary data")).unwrap());
        let new = json!({"a":2,"new":null});
        let before = settings_view(&schema, old.clone());
        let after = settings_view(&schema, new.clone());
        let mut field = FieldChange::new(
            "worker",
            "/setting",
            RecordedValue::new(old),
            RecordedValue::new(new),
            ApplyKind::ModuleRestart,
        );
        field.old_source = Layer::User;
        field.new_source = Layer::User;
        assert_eq!(
            checked_fields(
                "worker",
                &schema,
                Some(&before),
                Some(&schema),
                &after,
                ConfigReviewReply::new(vec![field.clone()], vec![])
            )
            .unwrap(),
            vec![field]
        );
    }

    #[test]
    fn config_review_migration_from_opaque_json_removes_whole_old_value() {
        let old_schema = settings_schema(true);
        let new_schema = settings_schema(false);
        let value = json!({"a":1});
        let before = settings_view(&old_schema, value.clone());
        let after = settings_view(&new_schema, value.clone());
        let fields = expected_fields(
            "worker",
            &new_schema,
            Some(&before),
            Some(&old_schema),
            &after,
        )
        .unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields["/setting"].old, RecordedValue::new(value));
        assert_eq!(fields["/setting"].new, RecordedValue::new(Value::Null));
        assert_eq!(fields["/setting"].apply, ApplyKind::ModuleRestart);
        assert_eq!(fields["/setting/a"].old, RecordedValue::new(Value::Null));
        assert_eq!(fields["/setting/a"].new, RecordedValue::new(json!(1)));
        assert_eq!(fields["/setting/a"].apply, ApplyKind::Live);
    }

    #[test]
    fn config_review_migration_to_opaque_json_adds_whole_new_value() {
        let old_schema = settings_schema(false);
        let new_schema = settings_schema(true);
        let value = json!({"a":1});
        let before = settings_view(&old_schema, value.clone());
        let after = settings_view(&new_schema, value.clone());
        let fields = expected_fields(
            "worker",
            &new_schema,
            Some(&before),
            Some(&old_schema),
            &after,
        )
        .unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields["/setting"].old, RecordedValue::new(Value::Null));
        assert_eq!(fields["/setting"].new, RecordedValue::new(value));
        assert_eq!(fields["/setting"].apply, ApplyKind::ModuleRestart);
        assert_eq!(fields["/setting/a"].old, RecordedValue::new(json!(1)));
        assert_eq!(fields["/setting/a"].new, RecordedValue::new(Value::Null));
        assert_eq!(fields["/setting/a"].apply, ApplyKind::Live);
    }

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
    fn config_review_resolves_user_pattern_fields_in_module_and_project_views() {
        let schema = Schema::new(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object", "unevaluatedProperties": false,
            "properties": {"servers": {
                "type": "object", "unevaluatedProperties": false, "x-ck-layers": ["user"],
                "patternProperties": {"^[a-z]+$": {
                    "type": "object", "unevaluatedProperties": false,
                    "properties": {"command": {
                        "type": "string", "description": "Program to launch",
                        "x-ck-required": true, "x-ck-apply": "module_restart"
                    }}
                }}
            }}
        }))
        .unwrap();
        let view = |generation, command, project_id: Option<&str>| {
            let project_id = project_id.map(str::to_owned);
            let tuple = schema
                .review_tuple(
                    &[subc_config::LayerValue::new(
                        Layer::User,
                        json!({"servers":{"fixture":{"command":command}}}),
                    )],
                    &subc_config::ValidationContext::new("worker", project_id.clone()),
                )
                .unwrap();
            let mut view = ConfigView::new(
                generation,
                tuple.effective,
                tuple.sources,
                vec![],
                ConfigStatus::Active,
            );
            view.project_id = project_id;
            view
        };
        for project_id in [Some("project"), None] {
            let before = view(1, "old", project_id);
            let after = view(2, "new", project_id);
            let mut field = FieldChange::new(
                "worker",
                "/servers/fixture/command",
                RecordedValue::new(json!("old")),
                RecordedValue::new(json!("new")),
                ApplyKind::ModuleRestart,
            );
            field.project_id = after.project_id.clone();
            field.old_source = Layer::User;
            field.new_source = Layer::User;
            assert_eq!(
                checked_fields(
                    "worker",
                    &schema,
                    Some(&before),
                    Some(&schema),
                    &after,
                    ConfigReviewReply::new(vec![field.clone()], vec![]),
                )
                .unwrap(),
                vec![field]
            );
        }
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
