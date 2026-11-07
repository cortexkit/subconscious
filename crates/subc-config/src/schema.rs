use crate::{
    canonical_json, digest, ApplyKind, Bound, Digest, Layer, RecordedValue, Refusal, RefusalCode,
    PURPOSE_SCHEMA, PURPOSE_VALUE,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const VOCABULARY_META_SCHEMA: &str = include_str!("../vocabulary.schema.json");

/// Approved schema, with local references expanded and use-site policies inherited.
/// Full draft 2020-12 and cross-field validation remain the module's review duty.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Schema {
    normalized: Value,
}

#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ValidationContext {
    pub module_id: String,
    pub project_id: Option<String>,
    /// Closed picker sets are pinned local evidence, never fetched during review.
    pub closed_options: BTreeMap<String, Vec<Value>>,
    /// Project-scoped credentials, attested for this module and project by the caller.
    pub project_credentials: BTreeSet<Digest>,
    /// Credential/destination relationships are module policy, not inferred from key names.
    pub credential_destinations: BTreeMap<String, Vec<String>>,
}
impl ValidationContext {
    pub fn new(module_id: impl Into<String>, project_id: Option<String>) -> Self {
        Self {
            module_id: module_id.into(),
            project_id,
            ..Self::default()
        }
    }
}

/// Immutable layer proposal input. A project id is mandatory for project reviews.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct LayerValue {
    pub layer: Layer,
    pub value: Value,
}
impl LayerValue {
    pub fn new(layer: Layer, value: Value) -> Self {
        Self { layer, value }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReviewedTuple {
    pub effective: Value,
    pub sources: BTreeMap<String, Layer>,
}
impl ReviewedTuple {
    pub fn new(effective: Value, sources: BTreeMap<String, Layer>) -> Self {
        Self { effective, sources }
    }
}

impl Schema {
    pub fn new(schema: Value) -> Result<Self, Refusal> {
        if schema.get("$schema").and_then(Value::as_str)
            != Some("https://json-schema.org/draft/2020-12/schema")
        {
            return Err(schema_error(""));
        }
        let normalized = normalize(&schema, &schema, None, &mut Vec::new(), "")?;
        if normalized.get("type").and_then(Value::as_str) != Some("object") {
            return Err(schema_error(""));
        }
        let result = Self { normalized };
        // A malformed or out-of-picker default is a declaration error, not a late
        // runtime surprise. Dynamic closed sets are checked when supplied to review.
        validate_defaults(&result.normalized, "")?;
        Ok(result)
    }
    pub fn normalized(&self) -> &Value {
        &self.normalized
    }
    pub fn digest(&self) -> Result<Digest, crate::CanonicalError> {
        digest(PURPOSE_SCHEMA, &self.normalized)
    }
    pub fn defaults(&self) -> Value {
        defaults(&self.normalized).unwrap_or_else(|| Value::Object(Map::new()))
    }
    pub fn validate_layer(
        &self,
        layer: Layer,
        value: &Value,
        context: &ValidationContext,
    ) -> Result<(), Refusal> {
        if layer == Layer::Default {
            return Err(refusal(RefusalCode::LayerForbidden, "", context));
        }
        validate(&self.normalized, value, "", Some(layer), true, context)
    }
    /// Module-side helper for the normative merge table. Call separately for
    /// every affected approved project, including projects not edited by a plan.
    pub fn review_tuple(
        &self,
        layers: &[LayerValue],
        context: &ValidationContext,
    ) -> Result<ReviewedTuple, Refusal> {
        let mut effective = self.defaults();
        let mut sources = BTreeMap::new();
        mark_sources(&effective, "", Layer::Default, &mut sources);
        let mut previous = Layer::Default;
        let mut user_credentials = BTreeSet::new();
        for proposal in layers {
            if proposal.layer <= previous
                || (proposal.layer == Layer::Project && context.project_id.is_none())
            {
                return Err(refusal(RefusalCode::LayerForbidden, "", context));
            }
            previous = proposal.layer;
            validate(
                &overlay_schema(&self.normalized, &effective, &proposal.value),
                &proposal.value,
                "",
                Some(proposal.layer),
                true,
                context,
            )?;
            if proposal.layer == Layer::User {
                collect_credentials(&self.normalized, &proposal.value, &mut user_credentials);
            }
            if matches!(proposal.layer, Layer::Project | Layer::Workspace) {
                check_credentials(
                    &overlay_schema(&self.normalized, &effective, &proposal.value),
                    &proposal.value,
                    "",
                    &user_credentials,
                    context,
                )?;
                for (credential_path, destinations) in &context.credential_destinations {
                    if effective
                        .pointer(credential_path)
                        .is_some_and(is_vault_reference)
                        && sources.get(credential_path) == Some(&Layer::User)
                        && proposal.value.pointer(credential_path).is_none()
                        && destinations
                            .iter()
                            .any(|p| proposal.value.pointer(p).is_some())
                    {
                        return Err(refusal(
                            RefusalCode::CredentialShared,
                            credential_path,
                            context,
                        ));
                    }
                }
            }
            merge(
                &self.normalized,
                &mut effective,
                &proposal.value,
                "",
                proposal.layer,
                &mut sources,
                context,
            )?;
        }
        validate(&self.normalized, &effective, "", None, false, context)?;
        Ok(ReviewedTuple::new(effective, sources))
    }
    /// The only schema-aware value projection intended for durable views and
    /// refusal records. Call before recording even invalid credential literals.
    pub fn record(&self, value: &Value) -> Result<Value, crate::CanonicalError> {
        record(&self.normalized, value)
    }
}

fn schema_error(path: &str) -> Refusal {
    Refusal::new(RefusalCode::SchemaInvalid, path, None)
}
fn refusal(code: RefusalCode, path: &str, c: &ValidationContext) -> Refusal {
    Refusal::new(code, path, c.project_id.clone())
}
fn pointer(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}
fn credential(s: &Value) -> bool {
    s.get("x-ck-kind").and_then(Value::as_str) == Some("credential_ref")
}

/// References contain opaque ids, not secret material, paths or env expansions.
pub fn is_vault_reference(value: &Value) -> bool {
    value
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.get("vault_ref"))
        .and_then(Value::as_str)
        .is_some_and(|s| {
            !s.is_empty() && s.len() <= 256 && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
        })
}

fn normalize(
    root: &Value,
    input: &Value,
    parent: Option<&Value>,
    stack: &mut Vec<String>,
    path: &str,
) -> Result<Value, Refusal> {
    let mut node = input
        .as_object()
        .cloned()
        .ok_or_else(|| schema_error(path))?;
    if let Some(reference) = node.remove("$ref") {
        let reference = reference.as_str().ok_or_else(|| schema_error(path))?;
        if !reference.starts_with("#/") || stack.iter().any(|s| s == reference) {
            return Err(schema_error(path));
        }
        let target = root
            .pointer(&reference[1..])
            .ok_or_else(|| schema_error(path))?;
        stack.push(reference.to_owned());
        let resolved = normalize(root, target, parent, stack, path)?;
        let mut expanded = resolved
            .as_object()
            .cloned()
            .ok_or_else(|| schema_error(path))?;
        for (key, value) in node {
            if key == "x-ck-layers" {
                if let Some(allowed) = expanded.get(&key).and_then(Value::as_array) {
                    if value
                        .as_array()
                        .is_none_or(|a| a.iter().any(|v| !allowed.contains(v)))
                    {
                        return Err(schema_error(path));
                    }
                }
            }
            if key == "x-ck-apply" {
                if let Some(existing) = expanded.get(&key) {
                    if existing != &value && existing != "live" && value != "daemon_restart" {
                        return Err(schema_error(path));
                    }
                }
            }
            if expanded.get(&key).is_some_and(|v| v != &value)
                && !key.starts_with("x-ck-")
                && !["description", "default", "$schema"].contains(&key.as_str())
            {
                return Err(schema_error(path));
            }
            expanded.insert(key, value);
        }
        let result = normalize(root, &Value::Object(expanded), parent, stack, path);
        stack.pop();
        return result;
    }
    if ["allOf", "anyOf", "if", "then", "else", "$dynamicRef"]
        .iter()
        .any(|k| node.contains_key(*k))
    {
        // Policy intersections and conditional policies cannot be skipped safely.
        return Err(schema_error(path));
    }
    if let Some(parent) = parent {
        for key in ["x-ck-layers", "x-ck-apply"] {
            if !node.contains_key(key) {
                if let Some(v) = parent.get(key) {
                    node.insert(key.to_owned(), v.clone());
                }
            }
        }
        if let (Some(p), Some(c)) = (
            parent.get("x-ck-layers").and_then(Value::as_array),
            node.get("x-ck-layers").and_then(Value::as_array),
        ) {
            if c.iter().any(|v| !p.contains(v)) {
                return Err(schema_error(path));
            }
        }
        if let (Some(p), Some(c)) = (parent.get("x-ck-apply"), node.get("x-ck-apply")) {
            if p != c && p != "live" && c != "daemon_restart" {
                return Err(schema_error(path));
            }
        }
    }
    let policy = Value::Object(node.clone());
    validate_vocabulary(&policy, path)?;
    if let Some(layers) = node.get("x-ck-layers") {
        let values = layers.as_array().ok_or_else(|| schema_error(path))?;
        if values.is_empty()
            || values.iter().any(|v| {
                ![
                    Value::from("user"),
                    Value::from("workspace"),
                    Value::from("project"),
                ]
                .contains(v)
            })
            || values
                .iter()
                .enumerate()
                .any(|(i, v)| values[..i].contains(v))
        {
            return Err(schema_error(path));
        }
    }
    if let Some(apply) = node.get("x-ck-apply") {
        serde_json::from_value::<ApplyKind>(apply.clone()).map_err(|_| schema_error(path))?;
        if ["module_restart", "daemon_restart"].contains(&apply.as_str().unwrap_or_default())
            && node.get("x-ck-layers") != Some(&serde_json::json!(["user"]))
        {
            return Err(schema_error(path));
        }
    }
    if let Some(branches) = node.get_mut("oneOf") {
        let branches = branches.as_array_mut().ok_or_else(|| schema_error(path))?;
        if branches.len() < 2 {
            return Err(schema_error(path));
        }
        for branch in branches.iter_mut() {
            *branch = normalize(root, branch, Some(&policy), stack, path)?;
        }
        discriminator(branches).ok_or_else(|| schema_error(path))?;
        return Ok(Value::Object(node));
    }
    let kind = node
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| schema_error(path))?
        .to_owned();
    if !["object", "array", "string", "number", "integer", "boolean"].contains(&kind.as_str()) {
        return Err(schema_error(path));
    }
    let structural = kind == "object" && !credential(&policy);
    if !structural
        && (node
            .get("description")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            || (!node.contains_key("default")
                && node.get("x-ck-required") != Some(&Value::Bool(true)))
            || !node.contains_key("x-ck-apply")
            || !node.contains_key("x-ck-layers"))
    {
        return Err(schema_error(path));
    }
    if let Some(bound) = node.get("x-ck-bound") {
        let bound: Bound = serde_json::from_value(bound.clone()).map_err(|_| schema_error(path))?;
        if bound == Bound::SubsetOnly && kind != "array" {
            return Err(schema_error(path));
        }
        if bound != Bound::SubsetOnly && !["number", "integer"].contains(&kind.as_str()) {
            let order = node
                .get("x-ck-ordering")
                .and_then(Value::as_array)
                .ok_or_else(|| schema_error(path))?;
            let values = if kind == "boolean" {
                serde_json::json!([false, true])
            } else {
                node.get("enum")
                    .cloned()
                    .ok_or_else(|| schema_error(path))?
            };
            let values = values.as_array().ok_or_else(|| schema_error(path))?;
            if (kind == "boolean" && order != values)
                || order.len() != values.len()
                || order.iter().any(|v| !values.contains(v))
                || order
                    .iter()
                    .enumerate()
                    .any(|(i, v)| order[..i].contains(v))
            {
                return Err(schema_error(path));
            }
        }
    }
    if structural {
        if node.get("unevaluatedProperties") != Some(&Value::Bool(false)) {
            return Err(schema_error(path));
        }
        if let Some(properties) = node.get_mut("properties") {
            for (key, child) in properties
                .as_object_mut()
                .ok_or_else(|| schema_error(path))?
            {
                *child = normalize(root, child, Some(&policy), stack, &pointer(path, key))?;
            }
        }
        if let Some(patterns) = node.get_mut("patternProperties") {
            let patterns = patterns.as_object_mut().ok_or_else(|| schema_error(path))?;
            if patterns.len() != 1 || node_has_open_map(&policy) {
                return Err(schema_error(path));
            }
            for (pattern, child) in patterns {
                Regex::new(pattern).map_err(|_| schema_error(path))?;
                *child = normalize(root, child, Some(&policy), stack, &pointer(path, "*"))?;
            }
        }
        if node
            .get("additionalProperties")
            .is_some_and(|v| v != &Value::Bool(false))
        {
            return Err(schema_error(path));
        }
    }
    if kind == "array" {
        let child = node.get_mut("items").ok_or_else(|| schema_error(path))?;
        *child = normalize(root, child, Some(&policy), stack, &pointer(path, "*"))?;
    }
    if let Some(pattern) = node.get("pattern") {
        Regex::new(pattern.as_str().ok_or_else(|| schema_error(path))?)
            .map_err(|_| schema_error(path))?;
    }
    node.remove("$defs");
    Ok(Value::Object(node))
}
fn node_has_open_map(node: &Value) -> bool {
    node.get("additionalProperties")
        .is_some_and(|v| v != &Value::Bool(false))
}

fn discriminator(branches: &[Value]) -> Option<String> {
    let first = branches.first()?.get("properties")?.as_object()?;
    let candidates: Vec<_> = first
        .iter()
        .filter(|(key, v)| {
            v.get("const").is_some()
                && branches.iter().all(|b| {
                    b.get("required")
                        .and_then(Value::as_array)
                        .is_some_and(|r| r.contains(&Value::String((*key).clone())))
                })
        })
        .filter(|(key, _)| {
            let constants: Vec<_> = branches
                .iter()
                .filter_map(|b| b.get("properties")?.get(*key)?.get("const"))
                .collect();
            constants.len() == branches.len()
                && constants
                    .iter()
                    .enumerate()
                    .all(|(i, v)| !constants[..i].contains(v))
        })
        .collect();
    if candidates.len() == 1 {
        Some(candidates[0].0.clone())
    } else {
        None
    }
}
fn variant<'a>(s: &'a Value, value: &Value) -> Option<&'a Value> {
    let branches = s.get("oneOf")?.as_array()?;
    let key = discriminator(branches)?;
    branches.iter().find(|b| {
        value
            .get(&key)
            .is_some_and(|v| b["properties"][&key]["const"] == *v)
    })
}
fn child_schema<'a>(s: &'a Value, key: &str) -> Option<&'a Value> {
    s.get("properties").and_then(|p| p.get(key)).or_else(|| {
        s.get("patternProperties")?
            .as_object()?
            .iter()
            .find(|(pattern, _)| Regex::new(pattern).is_ok_and(|r| r.is_match(key)))
            .map(|(_, v)| v)
    })
}
fn defaults(s: &Value) -> Option<Value> {
    if let Some(v) = s.get("default") {
        return Some(v.clone());
    }
    let properties = s.get("properties")?.as_object()?;
    Some(Value::Object(
        properties
            .iter()
            .filter_map(|(key, schema)| defaults(schema).map(|v| (key.clone(), v)))
            .collect(),
    ))
}
fn validate_defaults(s: &Value, path: &str) -> Result<(), Refusal> {
    if let Some(value) = s.get("default") {
        let mut declaration_schema = s.clone();
        strip_options(&mut declaration_schema);
        validate(
            &declaration_schema,
            value,
            path,
            None,
            false,
            &ValidationContext::default(),
        )
        .map_err(|_| schema_error(path))?;
    }
    if let Some(props) = s.get("properties").and_then(Value::as_object) {
        for (key, child) in props {
            validate_defaults(child, &pointer(path, key))?;
        }
    }
    if let Some(patterns) = s.get("patternProperties").and_then(Value::as_object) {
        for child in patterns.values() {
            validate_defaults(child, path)?;
        }
    }
    if let Some(items) = s.get("items") {
        validate_defaults(items, path)?;
    }
    if let Some(branches) = s.get("oneOf").and_then(Value::as_array) {
        for branch in branches {
            validate_defaults(branch, path)?;
        }
    }
    Ok(())
}

fn validate(
    s: &Value,
    value: &Value,
    path: &str,
    layer: Option<Layer>,
    partial: bool,
    c: &ValidationContext,
) -> Result<(), Refusal> {
    let invalid = || refusal(RefusalCode::ValueInvalid, path, c);
    if value.is_null() || canonical_json(value).is_err() || contains_expansion(value) {
        return Err(invalid());
    }
    if let Some(layer) = layer {
        if let Some(layers) = s.get("x-ck-layers").and_then(Value::as_array) {
            if !layers.contains(&serde_json::to_value(layer).expect("layer enum")) {
                return Err(refusal(RefusalCode::LayerForbidden, path, c));
            }
        }
    }
    if credential(s) {
        return if is_vault_reference(value) {
            Ok(())
        } else {
            Err(invalid())
        };
    }
    if s.get("oneOf").is_some() {
        if let Some(branch) = variant(s, value) {
            return validate(branch, value, path, layer, partial, c);
        }
        if partial {
            let branches = s["oneOf"].as_array().expect("normalized union");
            let key = discriminator(branches).expect("normalized discriminator");
            if value.get(&key).is_none() {
                // Without an inherited variant, a standalone layer check accepts
                // only overlays valid under every branch. Tuple review selects the
                // actual inherited branch before checking its policies again.
                for branch in branches {
                    validate(branch, value, path, layer, true, c)?;
                }
                return Ok(());
            }
        }
        return Err(invalid());
    }
    let matches = match s["type"].as_str() {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("boolean") => value.is_boolean(),
        Some("number") => value.is_number(),
        Some("integer") => value.as_f64().is_some_and(|n| n.fract() == 0.0),
        _ => false,
    };
    if !matches {
        return Err(invalid());
    }
    if s.get("const").is_some_and(|v| !same_scalar(v, value))
        || s.get("enum")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.iter().any(|v| same_scalar(v, value)))
    {
        return Err(invalid());
    }
    if let Some(options) = s.get("x-ck-options") {
        if options.get("closed") == Some(&Value::Bool(true)) {
            // Runtime closed-picker checks require pinned caller-supplied evidence;
            // a schema enum cannot substitute for an unavailable catalog.
            let set = c.closed_options.get(path);
            if set.is_none_or(|values| !values.iter().any(|v| same_scalar(v, value))) {
                return Err(invalid());
            }
        }
    }
    if let Some(text) = value.as_str() {
        if text.contains("{env:") || text.contains("{file:") {
            return Err(invalid());
        }
        let len = text.chars().count() as u64;
        if s.get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|n| len < n)
            || s.get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|n| len > n)
        {
            return Err(invalid());
        }
        if s.get("pattern")
            .and_then(Value::as_str)
            .is_some_and(|p| !Regex::new(p).expect("normalized pattern").is_match(text))
        {
            return Err(invalid());
        }
    }
    if let Some(n) = value.as_f64() {
        for (key, fails) in [
            ("minimum", false),
            ("maximum", false),
            ("exclusiveMinimum", true),
            ("exclusiveMaximum", true),
        ] {
            if let Some(bound) = s.get(key).and_then(Value::as_f64) {
                let is_min = key.ends_with("Minimum") || key == "minimum";
                if if is_min {
                    n < bound || (fails && n == bound)
                } else {
                    n > bound || (fails && n == bound)
                } {
                    return Err(invalid());
                }
            }
        }
    }
    if let Some(object) = value.as_object() {
        if object.keys().any(|k| k == "env" || k == "file") && object.len() == 1 {
            return Err(invalid());
        }
        for (key, child) in object {
            let child_path = pointer(path, key);
            let schema = child_schema(s, key)
                .ok_or_else(|| refusal(RefusalCode::ValueInvalid, &child_path, c))?;
            validate(schema, child, &child_path, layer, partial, c)?;
        }
        if !partial {
            if let Some(required) = s.get("required").and_then(Value::as_array) {
                for key in required {
                    let key = key.as_str().ok_or_else(invalid)?;
                    if !object.contains_key(key) {
                        return Err(refusal(RefusalCode::ValueInvalid, &pointer(path, key), c));
                    }
                }
            }
            if let Some(props) = s.get("properties").and_then(Value::as_object) {
                for (key, schema) in props {
                    if schema.get("x-ck-required") == Some(&Value::Bool(true))
                        && !object.contains_key(key)
                    {
                        return Err(refusal(RefusalCode::ValueInvalid, &pointer(path, key), c));
                    }
                }
            }
        }
    }
    if let Some(array) = value.as_array() {
        if s.get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|n| (array.len() as u64) < n)
            || s.get("maxItems")
                .and_then(Value::as_u64)
                .is_some_and(|n| array.len() as u64 > n)
        {
            return Err(invalid());
        }
        if s.get("uniqueItems") == Some(&Value::Bool(true))
            && array
                .iter()
                .enumerate()
                .any(|(i, v)| array[..i].iter().any(|x| same_scalar(x, v)))
        {
            return Err(invalid());
        }
        let ids_declared = s["items"]["properties"].get("id").is_some();
        let mut ids = Vec::new();
        for (i, child) in array.iter().enumerate() {
            if ids_declared {
                if let Some(id) = child.get("id") {
                    if ids.contains(id) {
                        return Err(invalid());
                    }
                    ids.push(id.clone());
                }
            }
            validate(
                &s["items"],
                child,
                &pointer(path, &i.to_string()),
                layer,
                false,
                c,
            )?;
        }
    }
    Ok(())
}
fn same_scalar(a: &Value, b: &Value) -> bool {
    match (canonical_json(a), canonical_json(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
fn bound_ok(s: &Value, lower: &Value, higher: &Value) -> bool {
    let Some(bound) = s.get("x-ck-bound") else {
        return true;
    };
    if bound == "subset_only" {
        let (Some(a), Some(b)) = (lower.as_array(), higher.as_array()) else {
            return false;
        };
        return a
            .iter()
            .chain(b)
            .all(|v| !v.is_object() && !v.is_array() && !v.is_null())
            && b.iter().all(|v| a.iter().any(|x| same_scalar(x, v)));
    }
    let comparison = if let Some(order) = s.get("x-ck-ordering").and_then(Value::as_array) {
        order
            .iter()
            .position(|v| same_scalar(v, higher))
            .zip(order.iter().position(|v| same_scalar(v, lower)))
            .map(|(h, l)| h.cmp(&l))
    } else {
        higher
            .as_f64()
            .zip(lower.as_f64())
            .and_then(|(h, l)| h.partial_cmp(&l))
    };
    comparison.is_some_and(|cmp| {
        if bound == "raise_only" {
            !cmp.is_lt()
        } else {
            !cmp.is_gt()
        }
    })
}
fn merge(
    s: &Value,
    lower: &mut Value,
    higher: &Value,
    path: &str,
    layer: Layer,
    sources: &mut BTreeMap<String, Layer>,
    c: &ValidationContext,
) -> Result<(), Refusal> {
    if !bound_ok(s, lower, higher) {
        return Err(refusal(RefusalCode::BoundViolated, path, c));
    }
    if credential(s) {
        *lower = higher.clone();
        sources.insert(path.into(), layer);
        return Ok(());
    }
    let schema = if let Some(branches) = s.get("oneOf").and_then(Value::as_array) {
        let key = discriminator(branches).expect("normalized union");
        if lower.get(&key).is_some()
            && higher.get(&key).is_some()
            && lower.get(&key) != higher.get(&key)
        {
            return Err(refusal(RefusalCode::ValueInvalid, &pointer(path, &key), c));
        }
        variant(s, higher)
            .or_else(|| variant(s, lower))
            .ok_or_else(|| refusal(RefusalCode::ValueInvalid, path, c))?
    } else {
        s
    };
    if let Some(object) = higher.as_object() {
        if !lower.is_object() {
            return Err(refusal(RefusalCode::ValueInvalid, path, c));
        }
        let target = lower.as_object_mut().expect("object");
        for (key, value) in object {
            let child = child_schema(schema, key)
                .ok_or_else(|| refusal(RefusalCode::ValueInvalid, &pointer(path, key), c))?;
            let child_path = pointer(path, key);
            if let Some(existing) = target.get_mut(key) {
                merge(child, existing, value, &child_path, layer, sources, c)?;
            } else {
                let mut initial = defaults(child).unwrap_or_else(|| {
                    if value.is_object() {
                        serde_json::json!({})
                    } else {
                        value.clone()
                    }
                });
                merge(child, &mut initial, value, &child_path, layer, sources, c)?;
                target.insert(key.clone(), initial);
            }
        }
    } else {
        if lower.is_object() || lower.is_array() != higher.is_array() {
            return Err(refusal(RefusalCode::ValueInvalid, path, c));
        }
        *lower = higher.clone();
        sources.retain(|p, _| p != path && !p.starts_with(&format!("{path}/")));
        mark_sources(higher, path, layer, sources);
    }
    Ok(())
}
fn mark_sources(v: &Value, path: &str, layer: Layer, sources: &mut BTreeMap<String, Layer>) {
    if let Some(object) = v.as_object() {
        if is_vault_reference(v) {
            sources.insert(path.into(), layer);
        } else {
            for (key, value) in object {
                mark_sources(value, &pointer(path, key), layer, sources);
            }
        }
    } else {
        sources.insert(path.into(), layer);
    }
}
fn collect_credentials(s: &Value, value: &Value, out: &mut BTreeSet<Digest>) {
    let s = variant(s, value).unwrap_or(s);
    if credential(s) {
        if let Ok(d) = digest(PURPOSE_VALUE, value) {
            out.insert(d);
        }
    } else if let Some(o) = value.as_object() {
        for (key, v) in o {
            if let Some(child) = child_schema(s, key) {
                collect_credentials(child, v, out);
            }
        }
    } else if let Some(a) = value.as_array() {
        for v in a {
            collect_credentials(&s["items"], v, out);
        }
    }
}
fn check_credentials(
    s: &Value,
    value: &Value,
    path: &str,
    user: &BTreeSet<Digest>,
    c: &ValidationContext,
) -> Result<(), Refusal> {
    let s = variant(s, value).unwrap_or(s);
    if credential(s) {
        let d = digest(PURPOSE_VALUE, value)
            .map_err(|_| refusal(RefusalCode::ValueInvalid, path, c))?;
        if c.project_id.is_none() || user.contains(&d) || !c.project_credentials.contains(&d) {
            return Err(refusal(RefusalCode::CredentialShared, path, c));
        }
    } else if let Some(o) = value.as_object() {
        for (key, v) in o {
            if let Some(child) = child_schema(s, key) {
                check_credentials(child, v, &pointer(path, key), user, c)?;
            }
        }
    } else if let Some(a) = value.as_array() {
        for (i, v) in a.iter().enumerate() {
            check_credentials(&s["items"], v, &pointer(path, &i.to_string()), user, c)?;
        }
    }
    Ok(())
}
fn record(s: &Value, value: &Value) -> Result<Value, crate::CanonicalError> {
    if credential(s) {
        return serde_json::to_value(RecordedValue::redacted(value)?)
            .map_err(|_| crate::CanonicalError::Serialization);
    }
    let schema = variant(s, value).unwrap_or(s);
    match value {
        Value::Object(o) => Ok(Value::Object(
            o.iter()
                .map(|(key, v)| {
                    // Unknown keys have no trusted type. Redact them rather than letting
                    // a rejected literal escape through an unrecognized schema path.
                    let v = if let Some(child) = child_schema(schema, key) {
                        record(child, v)?
                    } else {
                        serde_json::to_value(RecordedValue::redacted(v)?)
                            .map_err(|_| crate::CanonicalError::Serialization)?
                    };
                    Ok((key.clone(), v))
                })
                .collect::<Result<_, crate::CanonicalError>>()?,
        )),
        Value::Array(a) => Ok(Value::Array(
            a.iter()
                .map(|v| record(&schema["items"], v))
                .collect::<Result<_, _>>()?,
        )),
        other => Ok(other.clone()),
    }
}

fn strip_options(s: &mut Value) {
    match s {
        Value::Object(o) => {
            o.remove("x-ck-options");
            for value in o.values_mut() {
                strip_options(value);
            }
        }
        Value::Array(a) => {
            for value in a {
                strip_options(value);
            }
        }
        _ => {}
    }
}
fn validate_vocabulary(node: &Value, path: &str) -> Result<(), Refusal> {
    let vocabulary: Value =
        serde_json::from_str(VOCABULARY_META_SCHEMA).expect("committed vocabulary");
    for (key, value) in node.as_object().expect("schema object") {
        if !key.starts_with("x-ck-") {
            continue;
        }
        let definition = vocabulary["properties"]
            .get(key)
            .ok_or_else(|| schema_error(path))?;
        if let Some(values) = definition.get("enum").and_then(Value::as_array) {
            if !values.contains(value) {
                return Err(schema_error(path));
            }
        }
        if let Some(t) = definition.get("type").and_then(Value::as_str) {
            let valid = match t {
                "object" => value.is_object(),
                "array" => value.is_array(),
                "string" => value.is_string(),
                "boolean" => value.is_boolean(),
                "number" => value.is_number(),
                _ => false,
            };
            if !valid {
                return Err(schema_error(path));
            }
        }
    }
    if let Some(options) = node.get("x-ck-options") {
        let o = options.as_object().ok_or_else(|| schema_error(path))?;
        if o.keys().any(|k| {
            ![
                "source",
                "items_pointer",
                "value_pointer",
                "label_pointer",
                "depends_on",
                "closed",
                "items_from_parent_pointer",
            ]
            .contains(&k.as_str())
        }) {
            return Err(schema_error(path));
        }
        for p in ["items_pointer", "value_pointer", "label_pointer"] {
            if !options
                .get(p)
                .and_then(Value::as_str)
                .is_some_and(valid_pointer)
            {
                return Err(schema_error(path));
            }
        }
        if !options.get("closed").is_some_and(Value::is_boolean) {
            return Err(schema_error(path));
        }
        let deps = options
            .get("depends_on")
            .and_then(Value::as_array)
            .ok_or_else(|| schema_error(path))?;
        if deps.iter().any(|p| !p.as_str().is_some_and(valid_pointer)) {
            return Err(schema_error(path));
        }
        let source = options
            .get("source")
            .and_then(Value::as_object)
            .ok_or_else(|| schema_error(path))?;
        if source
            .keys()
            .any(|k| !["module", "op", "params"].contains(&k.as_str()))
            || !["module", "op"].iter().all(|k| {
                source
                    .get(*k)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            })
            || !source.get("params").is_some_and(Value::is_object)
        {
            return Err(schema_error(path));
        }
    }
    Ok(())
}
fn valid_pointer(s: &str) -> bool {
    if !s.is_empty() && !s.starts_with('/') {
        return false;
    }
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}

fn overlay_schema(s: &Value, lower: &Value, higher: &Value) -> Value {
    let selected = variant(s, higher)
        .or_else(|| variant(s, lower))
        .unwrap_or(s);
    let mut result = selected.clone();
    if let Some(properties) = result.get_mut("properties").and_then(Value::as_object_mut) {
        for (key, child) in properties {
            *child = overlay_schema(child, &lower[key], &higher[key]);
        }
    }
    result
}

fn contains_expansion(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("{env:") || s.contains("{file:"),
        Value::Array(a) => a.iter().any(contains_expansion),
        Value::Object(o) => o
            .iter()
            .any(|(k, v)| k.contains("{env:") || k.contains("{file:") || contains_expansion(v)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn field(kind: &str, default: Value) -> Value {
        json!({"type":kind,"description":"A configurable value", "default":default,"x-ck-apply":"live","x-ck-layers":["user","workspace","project"]})
    }
    fn object(properties: Value) -> Value {
        json!({"type":"object","properties":properties,"unevaluatedProperties":false})
    }
    fn schema(properties: Value) -> Schema {
        let mut value = object(properties);
        value["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        Schema::new(value).unwrap()
    }
    fn context() -> ValidationContext {
        ValidationContext::new("module", Some("project-one".into()))
    }
    fn layers(user: Value, project: Value) -> Vec<LayerValue> {
        vec![
            LayerValue::new(Layer::User, user),
            LayerValue::new(Layer::Project, project),
        ]
    }

    #[test]
    fn policy_contract_raise_only_golden_names_affected_project() {
        let mut n = field("number", json!(5));
        n["x-ck-bound"] = json!("raise_only");
        let s = schema(json!({"n":n}));
        let valid = s
            .review_tuple(&layers(json!({"n":10}), json!({"n":15})), &context())
            .unwrap();
        assert_eq!(valid.effective, json!({"n":15}));
        assert_eq!(valid.sources["/n"], Layer::Project);
        let refusal = s
            .review_tuple(&layers(json!({"n":20}), json!({"n":15})), &context())
            .unwrap_err();
        assert_eq!(refusal.code, RefusalCode::BoundViolated);
        assert_eq!(refusal.path, "/n");
        assert_eq!(refusal.project_id.as_deref(), Some("project-one"));
        assert!(s
            .review_tuple(&layers(json!({"n":20}), json!({"n":25})), &context())
            .is_ok());
        assert!(s
            .review_tuple(&layers(json!({"n":20}), json!({})), &context())
            .is_ok());
    }
    #[test]
    fn policy_contract_lower_subset_and_ordered_bounds() {
        let mut n = field("number", json!(20));
        n["x-ck-bound"] = json!("lower_only");
        let s = schema(json!({"n":n}));
        assert!(s
            .review_tuple(&layers(json!({"n":10}), json!({"n":8})), &context())
            .is_ok());
        assert_eq!(
            s.review_tuple(&layers(json!({"n":10}), json!({"n":12})), &context())
                .unwrap_err()
                .code,
            RefusalCode::BoundViolated
        );
        let mut list = field("array", json!(["a", "b"]));
        list["items"] = field("string", json!("a"));
        list["x-ck-bound"] = json!("subset_only");
        let s = schema(json!({"list":list}));
        assert!(s
            .review_tuple(
                &layers(json!({"list":["b","a"]}), json!({"list":["b"]})),
                &context()
            )
            .is_ok());
        assert_eq!(
            s.review_tuple(
                &layers(json!({"list":["b"]}), json!({"list":["a"]})),
                &context()
            )
            .unwrap_err()
            .code,
            RefusalCode::BoundViolated
        );
        let mut on = field("boolean", json!(false));
        on["x-ck-bound"] = json!("raise_only");
        on["x-ck-ordering"] = json!([false, true]);
        let s = schema(json!({"on":on}));
        assert_eq!(
            s.review_tuple(&layers(json!({"on":true}), json!({"on":false})), &context())
                .unwrap_err()
                .code,
            RefusalCode::BoundViolated
        );
    }
    #[test]
    fn policy_contract_picker_and_secret_redaction() {
        let mut choice = field("string", json!("small"));
        choice["enum"] = json!(["small", "large"]);
        let mut secret = field("object", json!({"vault_ref":"user-key"}));
        secret["x-ck-kind"] = json!("credential_ref");
        let s = schema(json!({"choice":choice,"key":secret}));
        assert_eq!(
            s.validate_layer(Layer::User, &json!({"choice":"huge"}), &context())
                .unwrap_err()
                .code,
            RefusalCode::ValueInvalid
        );
        let literal = json!({"key":"must-never-be-retained"});
        let error = s
            .validate_layer(Layer::User, &literal, &context())
            .unwrap_err();
        assert_eq!(error.code, RefusalCode::ValueInvalid);
        let recorded = s.record(&literal).unwrap();
        let text = serde_json::to_string(&(recorded.clone(), error)).unwrap();
        assert!(!text.contains("must-never-be-retained"));
        assert!(text.contains("redacted"));
        assert_eq!(
            recorded["key"]["digest"],
            json!(digest(PURPOSE_VALUE, &literal["key"]).unwrap())
        );
        let accepted =
            serde_json::to_string(&s.record(&json!({"key":{"vault_ref":"user-key"}})).unwrap())
                .unwrap();
        assert!(!accepted.contains("user-key"));
        for invalid in [
            json!({"vault_ref":""}),
            json!({"vault_ref":"has spaces"}),
            json!({"vault_ref":"ok","secret":"bad"}),
            json!({"env":"KEY"}),
        ] {
            assert!(!is_vault_reference(&invalid));
        }
    }
    #[test]
    fn policy_contract_closed_picker_requires_pinned_evidence() {
        let mut choice = field("string", json!("small"));
        choice["x-ck-options"] = json!({"source":{"module":"catalog","op":"list","params":{}},"items_pointer":"/items","value_pointer":"/id","label_pointer":"/label","depends_on":[],"closed":true});
        let s = schema(json!({"choice":choice}));
        assert!(s
            .validate_layer(Layer::User, &json!({"choice":"small"}), &context())
            .is_err());
        let mut c = context();
        c.closed_options
            .insert("/choice".into(), vec![json!("small")]);
        assert!(s
            .validate_layer(Layer::User, &json!({"choice":"small"}), &c)
            .is_ok());
        assert!(s
            .validate_layer(Layer::User, &json!({"choice":"huge"}), &c)
            .is_err());
    }
    #[test]
    fn policy_contract_credential_shared_is_not_entitlement() {
        let mut key = field("object", json!({"vault_ref":"user-key"}));
        key["x-ck-kind"] = json!("credential_ref");
        let s = schema(json!({"key":key,"endpoint":field("string",json!("approved"))}));
        let mut c = context();
        c.credential_destinations
            .insert("/key".into(), vec!["/endpoint".into()]);
        let copied = layers(
            json!({"key":{"vault_ref":"user-key"}}),
            json!({"key":{"vault_ref":"user-key"}}),
        );
        c.project_credentials
            .insert(digest(PURPOSE_VALUE, &json!({"vault_ref":"user-key"})).unwrap());
        assert_eq!(
            s.review_tuple(&copied, &c).unwrap_err().code,
            RefusalCode::CredentialShared
        );
        let redirected = layers(
            json!({"key":{"vault_ref":"user-key"}}),
            json!({"endpoint":"other"}),
        );
        assert_eq!(
            s.review_tuple(&redirected, &c).unwrap_err().code,
            RefusalCode::CredentialShared
        );
        let owned = layers(
            json!({"key":{"vault_ref":"user-key"}}),
            json!({"key":{"vault_ref":"project-key"}}),
        );
        assert_eq!(
            s.review_tuple(&owned, &c).unwrap_err().code,
            RefusalCode::CredentialShared
        );
        c.project_credentials
            .insert(digest(PURPOSE_VALUE, &json!({"vault_ref":"project-key"})).unwrap());
        assert!(s.review_tuple(&owned, &c).is_ok());
    }
    #[test]
    fn policy_contract_merge_absent_empty_arrays_maps_null_and_unknown() {
        let mut array = field("array", json!([1, 2]));
        array["items"] = field("number", json!(0));
        let mut map = object(json!({}));
        map["patternProperties"] = json!({"^[a-z]+$":field("string",json!(""))});
        let s = schema(
            json!({"n":field("number",json!(4)),"on":field("boolean",json!(true)),"section":object(json!({"a":field("number",json!(1)),"b":field("number",json!(2))})),"v":array,"tags":map}),
        );
        let result = s
            .review_tuple(
                &layers(
                    json!({"tags":{"a":"x"}}),
                    json!({"on":false,"section":{"a":3},"v":[],"tags":{"b":"y"}}),
                ),
                &context(),
            )
            .unwrap();
        assert_eq!(
            result.effective,
            json!({"n":4,"on":false,"section":{"a":3,"b":2},"v":[],"tags":{"a":"x","b":"y"}})
        );
        assert_eq!(result.sources["/n"], Layer::Default);
        for invalid in [
            json!({"n":null}),
            json!({"unknown":1}),
            json!({"n":"4"}),
            json!({"tags":{"BAD":"x"}}),
        ] {
            assert!(s
                .validate_layer(Layer::Project, &invalid, &context())
                .is_err());
        }
    }
    #[test]
    fn policy_contract_union_inheritance_and_complete_array_items() {
        let mut disk_kind = field("string", json!("disk"));
        disk_kind["const"] = json!("disk");
        let mut memory_kind = field("string", json!("memory"));
        memory_kind["const"] = json!("memory");
        let mut disk = object(json!({"kind":disk_kind,"path":field("string",json!("a"))}));
        disk["required"] = json!(["kind", "path"]);
        let mut memory = object(json!({"kind":memory_kind,"capacity":field("number",json!(1))}));
        memory["required"] = json!(["kind", "capacity"]);
        let union = json!({"oneOf":[disk,memory],"default":{"kind":"disk","path":"a"}});
        let s = schema(json!({"storage":union}));
        let result = s
            .review_tuple(
                &layers(json!({}), json!({"storage":{"path":"b"}})),
                &context(),
            )
            .unwrap();
        assert_eq!(
            result.effective,
            json!({"storage":{"kind":"disk","path":"b"}})
        );
        assert!(s
            .review_tuple(
                &layers(json!({}), json!({"storage":{"kind":"memory","capacity":2}})),
                &context()
            )
            .is_err());
        let mut item =
            object(json!({"id":field("string",json!("a")),"n":field("number",json!(1))}));
        item["required"] = json!(["id", "n"]);
        let mut list = field("array", json!([]));
        list["items"] = item;
        let s = schema(json!({"list":list}));
        assert!(s
            .validate_layer(Layer::Project, &json!({"list":[{"id":"a"}]}), &context())
            .is_err());
        assert!(s
            .validate_layer(
                Layer::Project,
                &json!({"list":[{"id":"a","n":1},{"id":"a","n":2}]}),
                &context()
            )
            .is_err());
    }
    #[test]
    fn policy_contract_references_narrowing_and_bad_schemas() {
        let mut root = object(json!({"n":{"$ref":"#/$defs/n","x-ck-layers":["user"]}}));
        root["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        root["$defs"] = json!({"n":field("number",json!(5))});
        let s = Schema::new(root.clone()).unwrap();
        assert_eq!(
            s.validate_layer(Layer::Project, &json!({"n":6}), &context())
                .unwrap_err()
                .code,
            RefusalCode::LayerForbidden
        );
        root["properties"]["n"]["$ref"] = json!("https://example.invalid/schema");
        assert!(Schema::new(root.clone()).is_err());
        root["properties"]["n"]["$ref"] = json!("#/$defs/n");
        root["$defs"]["n"] = json!({"$ref":"#/$defs/n"});
        assert!(Schema::new(root).is_err());
        let mut boolean = field("boolean", json!(false));
        boolean["x-ck-bound"] = json!("raise_only");
        let mut root = object(json!({"on":boolean}));
        root["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        assert!(Schema::new(root).is_err());
        let mut root = object(json!({"n":field("number",json!(5))}));
        root["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        root["x-ck-layers"] = json!(["user"]);
        assert!(Schema::new(root).is_err());
    }
}
