use super::*;
use serde_json::{json, Value};
use subc_client_rs::config::{
    ApplyKind, ConfigReviewRefusal, FieldChange, LayerValue, RecordedValue, Schema,
    ValidationContext,
};

pub(super) fn document() -> Value {
    let layer = json!({
        "surface_mode": leaf(json!({"type":"string", "enum":["full", "search"], "default":"full"}), "Expose tools directly or through search"),
        "refresh": leaf(json!({"type":"string", "enum":["on_attach", "immediate"], "default":"on_attach"}), "When settings changes affect attached hosts"),
        "prompts": object("Prompt visibility", json!({
            "default_enabled": boolean("Show prompts without an override", Some(false)),
            "overrides": map("Overrides by prompt name", object("Prompt override", json!({
                "enabled": boolean("Show this prompt", None),
            }))),
        })),
        "providers": map("Providers by module id", object("Provider settings", json!({
            "enabled": boolean("Expose this provider", None),
            "namespace": leaf(json!({"type":"string"}), "Tool name prefix; defaults to the module id"),
            "tools": object("Tool policy", json!({
                "default_enabled": boolean("Expose tools without an override", Some(true)),
                "overrides": map("Overrides by tool name", object("Tool override", json!({
                    "enabled": boolean("Expose this tool", None),
                    "description": leaf(json!({"type":"string"}), "Replace the provider's tool description"),
                    "mode": leaf(json!({"type":"string", "enum":["forward", "ack_only"], "default":"forward"}), "Forward calls or acknowledge them locally"),
                    "upfront": boolean("Expose this tool before search", None),
                }))),
            })),
        }))),
    });
    let mut properties = layer.clone();
    properties["harness"] = map(
        "Overlays by app name",
        object("App settings overlay", layer),
    );
    let mut root = object("MCP gateway settings", properties);
    root["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
    root.as_object_mut().unwrap().remove("x-ck-required");
    root
}

fn leaf(mut shape: Value, description: &str) -> Value {
    shape["description"] = json!(description);
    shape["x-ck-layers"] = json!(["user", "project"]);
    shape["x-ck-apply"] = json!("live");
    if shape.get("default").is_none() {
        shape["x-ck-required"] = json!(false);
    }
    shape
}

fn boolean(description: &str, default: Option<bool>) -> Value {
    let mut shape = json!({"type":"boolean"});
    if let Some(default) = default {
        shape["default"] = json!(default);
    }
    leaf(shape, description)
}

fn object(description: &str, properties: Value) -> Value {
    leaf(
        json!({"type":"object", "unevaluatedProperties":false, "properties":properties}),
        description,
    )
}

fn map(description: &str, children: Value) -> Value {
    leaf(
        json!({"type":"object", "unevaluatedProperties":false, "patternProperties":{"^.+$":children}}),
        description,
    )
}

pub(super) fn schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(|| Schema::new(document()).expect("gateway settings schema must be valid"))
}

pub(super) fn review(module_id: &str, request: &ConfigReviewRequest) -> ConfigReviewReply {
    let project = request
        .layers
        .iter()
        .find_map(|layer| layer.project_id.clone())
        .or_else(|| {
            request
                .base
                .as_ref()
                .and_then(|base| base.project_id.clone())
        });
    match review_fields(module_id, project.clone(), request) {
        Ok(fields) => ConfigReviewReply::new(fields, vec![]),
        Err(error) => {
            let mut refusal =
                ConfigReviewRefusal::new("value_invalid", vec![], vec![error.to_string()]);
            refusal.project_id = project;
            ConfigReviewReply::new(vec![], vec![refusal])
        }
    }
}

fn review_fields(
    module_id: &str,
    project: Option<String>,
    request: &ConfigReviewRequest,
) -> Result<Vec<FieldChange>> {
    SettingsView::from_layers(&request.layers)?;
    let layers = request
        .layers
        .iter()
        .filter_map(|layer| layer.bytes.as_ref().map(|bytes| (layer, bytes)))
        .map(|(layer, bytes)| {
            let document =
                subc_jsonc::jsonc_to_json(std::str::from_utf8(bytes)?).map_err(other_error)?;
            Ok(LayerValue::new(
                layer
                    .layer
                    .known_layer()
                    .ok_or_else(|| other_error("unknown settings layer"))?,
                serde_json::from_str(&document)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let tuple = schema()
        .review_tuple(&layers, &ValidationContext::new(module_id, project.clone()))
        .map_err(|error| {
            other_error(format!(
                "settings do not match the gateway schema at {}: {:?}",
                error.path, error.code
            ))
        })?;
    let base = request.base.as_ref();
    let old = base.map_or(&Value::Null, |base| &base.effective);
    let new = &tuple.effective;
    let mut paths = BTreeSet::new();
    for value in [old, new] {
        leaf_paths(value, "", &mut paths);
    }
    let mut fields = Vec::new();
    for path in paths {
        let old_value = old.pointer(&path);
        let new_value = new.pointer(&path);
        let old_source = base.map_or(Layer::Default, |base| source(&base.sources, &path));
        let new_source = source(&tuple.sources, &path);
        if old_value == new_value && old_source == new_source {
            continue;
        }
        let mut field = FieldChange::new(
            module_id,
            &path,
            RecordedValue::new(old_value.cloned().unwrap_or(Value::Null)),
            RecordedValue::new(new_value.cloned().unwrap_or(Value::Null)),
            ApplyKind::Live,
        );
        field.old_source = old_source;
        field.new_source = new_source;
        field.project_id = project.clone();
        fields.push(field);
    }
    // Review records must describe the daemon's ordinary tuple. The gateway
    // narrows that tuple at session composition time, not during approval.
    Ok(fields)
}

fn leaf_paths(value: &Value, path: &str, paths: &mut BTreeSet<String>) {
    if let Some(object) = value.as_object().filter(|object| !object.is_empty()) {
        for (key, value) in object {
            leaf_paths(
                value,
                &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                paths,
            );
        }
    } else if !path.is_empty() {
        paths.insert(path.to_owned());
    }
}

fn source(sources: &BTreeMap<String, Layer>, path: &str) -> Layer {
    if let Some((_, layer)) = sources
        .iter()
        .find(|(key, _)| key.starts_with(&format!("{path}/")))
    {
        return *layer;
    }
    let mut path = path;
    loop {
        if let Some(layer) = sources.get(path) {
            return *layer;
        }
        let Some((parent, _)) = path.rsplit_once('/') else {
            return Layer::Default;
        };
        path = parent;
    }
}
