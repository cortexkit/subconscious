use crate::{
    canonical_json, digest, ApplyKind, Bound, Digest, Layer, RecordedValue, Refusal, RefusalCode,
    PURPOSE_SCHEMA, PURPOSE_VALUE,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

mod opaque;

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
        if is_opaque(&normalized)
            || normalized.get("type").and_then(Value::as_str) != Some("object")
        {
            return Err(schema_error(""));
        }
        let result = Self { normalized };
        // A malformed or out-of-picker default is a declaration error, not a late
        // runtime surprise. Dynamic closed sets are checked when supplied to review.
        validate_defaults(&result.normalized, "")?;
        validate_setup(&result.normalized, "", 0, false, &mut BTreeSet::new())?;
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
        mark_sources(
            &self.normalized,
            &effective,
            "",
            Layer::Default,
            &mut sources,
        );
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
fn is_opaque(s: &Value) -> bool {
    s.get("x-ck-kind").and_then(Value::as_str) == Some("json")
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
    let opaque = is_opaque(input);
    if let Some(reference) = (!opaque).then(|| node.remove("$ref")).flatten() {
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
    if !opaque
        && ["allOf", "anyOf", "if", "then", "else", "$dynamicRef"]
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
    if opaque {
        let user_only = serde_json::json!(["user"]);
        let layers = node.get("x-ck-layers").and_then(Value::as_array);
        if layers.is_none_or(|allowed| !allowed.iter().any(|layer| layer == "user"))
            || (node.contains_key("x-ck-bound") && node.get("x-ck-layers") != Some(&user_only))
        {
            return Err(schema_error(path));
        }
        node.insert("x-ck-layers".into(), user_only);
    }
    if let Some(apply) = node.get("x-ck-apply") {
        serde_json::from_value::<ApplyKind>(apply.clone()).map_err(|_| schema_error(path))?;
        if ["module_restart", "daemon_restart"].contains(&apply.as_str().unwrap_or_default())
            && node.get("x-ck-layers") != Some(&serde_json::json!(["user"]))
        {
            return Err(schema_error(path));
        }
    }
    let types = node.get("type").and_then(Value::as_array);
    if let Some(types) = types.filter(|_| !opaque) {
        if credential(&policy)
            || node.contains_key("oneOf")
            || types.is_empty()
            || types.iter().enumerate().any(|(i, t)| {
                !matches!(
                    t.as_str(),
                    Some("string" | "number" | "integer" | "boolean")
                ) || types[..i].contains(t)
            })
        {
            return Err(schema_error(path));
        }
    }
    if let Some(branches) = node.get_mut("oneOf").filter(|_| !opaque) {
        let branches = branches.as_array_mut().ok_or_else(|| schema_error(path))?;
        if branches.len() < 2 {
            return Err(schema_error(path));
        }
        for branch in branches.iter_mut() {
            *branch = normalize(root, branch, Some(&policy), stack, path)?;
        }
        if branches.iter().any(is_opaque) || discriminator(branches).is_none() {
            return Err(schema_error(path));
        }
        return Ok(Value::Object(node));
    }
    let kind = node
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let is_credential = credential(&policy);
    let types = node.get("type").and_then(Value::as_array);
    if !opaque
        && types.is_none()
        && !matches!(
            kind.as_str(),
            "object" | "array" | "string" | "number" | "integer" | "boolean"
        )
    {
        return Err(schema_error(path));
    }
    let structural = kind == "object" && !is_credential && !opaque;
    if !structural
        && (node
            .get("description")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            || (!is_credential
                && !node.contains_key("default")
                && !node.contains_key("x-ck-required"))
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
        let numeric = |kind: &str| matches!(kind, "number" | "integer");
        if bound != Bound::SubsetOnly
            && !numeric(&kind)
            && !types.is_some_and(|types| {
                !types.is_empty() && types.iter().all(|t| t.as_str().is_some_and(numeric))
            })
        {
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
    if opaque {
        let mut schema = Value::Object(node);
        opaque::normalize(root, &mut schema, stack, path, true)?;
        return Ok(schema);
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
    if is_opaque(s) {
        return None;
    }
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
    if credential(s) {
        return None;
    }
    if let Some(v) = s.get("default") {
        return Some(v.clone());
    }
    if is_opaque(s) {
        return None;
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
            Some(Layer::Default),
            false,
            &ValidationContext::default(),
        )
        .map_err(|_| schema_error(path))?;
    }
    if is_opaque(s) {
        return Ok(());
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

fn validate_setup(
    s: &Value,
    path: &str,
    depth: usize,
    named_property: bool,
    features: &mut BTreeSet<String>,
) -> Result<(), Refusal> {
    if let Some(setup) = s.get("x-ck-setup") {
        // Only top-level named objects are features, so one feature's section can
        // never contain another's. Allowing nested sections later only admits
        // schemas that are refused today, so a client built for top-level
        // features keeps reading every schema it can read now.
        if depth != 1
            || !named_property
            || s.get("type").and_then(Value::as_str) != Some("object")
            || s.get("oneOf").is_some()
            || credential(s)
            || is_opaque(s)
        {
            return Err(schema_error(path));
        }
        let setup = setup.as_object().ok_or_else(|| schema_error(path))?;
        let feature = setup
            .get("feature")
            .and_then(Value::as_str)
            .filter(|f| {
                Regex::new("^[a-z][a-z0-9_]{0,63}$")
                    .expect("feature pattern")
                    .is_match(f)
            })
            .ok_or_else(|| schema_error(path))?;
        if setup.len() != 3
            || setup
                .keys()
                .any(|key| !["feature", "order", "recommended"].contains(&key.as_str()))
            || !setup.get("order").is_some_and(|n| n.is_i64() || n.is_u64())
            || !features.insert(feature.to_owned())
        {
            return Err(schema_error(path));
        }
        let recommended = setup
            .get("recommended")
            .and_then(Value::as_object)
            .ok_or_else(|| schema_error(path))?;
        for (target, value) in recommended {
            let relative = target
                .strip_prefix(path)
                .filter(|p| p.starts_with('/'))
                .filter(|_| valid_pointer(target))
                .ok_or_else(|| schema_error(path))?;
            let mut field = s;
            for token in relative[1..].split('/') {
                if field.get("type").and_then(Value::as_str) != Some("object")
                    || field.get("oneOf").is_some()
                    || credential(field)
                    || is_opaque(field)
                {
                    return Err(schema_error(path));
                }
                let key = token.replace("~1", "/").replace("~0", "~");
                field = field
                    .get("properties")
                    .and_then(|props| props.get(&key))
                    .ok_or_else(|| schema_error(path))?;
            }
            if !is_opaque(field)
                && (field.get("type").and_then(Value::as_str) == Some("object")
                    || field.get("oneOf").is_some()
                    || schema_contains_credential(field))
            {
                return Err(schema_error(path));
            }
            // Like defaults, recommendations do not check membership in dynamic
            // x-ck-options catalogs at declaration. They are wizard proposals,
            // never daemon-applied default values.
            let mut declaration_schema = field.clone();
            strip_options(&mut declaration_schema);
            validate(
                &declaration_schema,
                value,
                target,
                None,
                false,
                &ValidationContext::default(),
            )
            .map_err(|_| schema_error(path))?;
        }
    }
    if is_opaque(s) {
        return Ok(());
    }
    for keyword in ["properties", "patternProperties"] {
        if let Some(properties) = s.get(keyword).and_then(Value::as_object) {
            // Visit keys in sorted order: serde_json keeps document order when
            // another crate in the build enables `preserve_order`, and the
            // duplicate-feature refusal must name the same section either way.
            let mut sorted: Vec<_> = properties.iter().collect();
            sorted.sort_unstable_by(|left, right| left.0.cmp(right.0));
            for (key, child) in sorted {
                validate_setup(
                    child,
                    &pointer(path, key),
                    depth + 1,
                    keyword == "properties",
                    features,
                )?;
            }
        }
    }
    if let Some(items) = s.get("items") {
        validate_setup(items, path, depth + 1, false, features)?;
    }
    if let Some(branches) = s.get("oneOf").and_then(Value::as_array) {
        for branch in branches {
            validate_setup(branch, path, depth + 1, false, features)?;
        }
    }
    Ok(())
}

fn schema_contains_credential(s: &Value) -> bool {
    credential(s)
        || ["properties", "patternProperties"].iter().any(|key| {
            s.get(*key)
                .and_then(Value::as_object)
                .is_some_and(|props| props.values().any(schema_contains_credential))
        })
        || s.get("items").is_some_and(schema_contains_credential)
        || s.get("oneOf")
            .and_then(Value::as_array)
            .is_some_and(|branches| branches.iter().any(schema_contains_credential))
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
    let opaque = is_opaque(s);
    if opaque && matches!(layer, Some(Layer::Workspace | Layer::Project)) {
        return Err(refusal(RefusalCode::LayerForbidden, path, c));
    }
    if value.is_null() || canonical_json(value).is_err() || contains_expansion(value) {
        return Err(invalid());
    }
    if let Some(layer) = layer.filter(|&layer| layer != Layer::Default) {
        if let Some(layers) = s.get("x-ck-layers").and_then(Value::as_array) {
            if !layers.contains(&serde_json::to_value(layer).expect("layer enum")) {
                return Err(refusal(RefusalCode::LayerForbidden, path, c));
            }
        }
    }
    if credential(s) {
        // A schema default cannot supply a vault reference, even inside an object or array.
        return if layer != Some(Layer::Default) && is_vault_reference(value) {
            Ok(())
        } else {
            Err(invalid())
        };
    }
    if !opaque && s.get("oneOf").is_some() {
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
    let matches = if opaque {
        opaque::matches(s, value)
    } else {
        matches_type(&s["type"], value)
            && s.get("const").is_none_or(|v| same_scalar(v, value))
            && s.get("enum")
                .and_then(Value::as_array)
                .is_none_or(|a| a.iter().any(|v| same_scalar(v, value)))
    };
    if !matches {
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
    if opaque {
        return Ok(());
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
fn matches_type(kind: &Value, value: &Value) -> bool {
    match kind.as_str() {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("boolean") => value.is_boolean(),
        Some("number") => value.is_number(),
        Some("integer") => value.as_f64().is_some_and(|n| n.fract() == 0.0),
        Some("null") => value.is_null(),
        _ => kind
            .as_array()
            .is_some_and(|types| types.iter().any(|kind| matches_type(kind, value))),
    }
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
    if credential(s) || is_opaque(s) {
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
            // Editing any field in a variant fixes it for higher layers, even
            // when that edit inherits the discriminator from the default.
            let prefix = format!("{path}/");
            if sources
                .get(path)
                .is_some_and(|&source| source != Layer::Default)
                || sources
                    .range(prefix.clone()..)
                    .take_while(|(p, _)| p.starts_with(&prefix))
                    .any(|(_, &source)| source != Layer::Default)
            {
                return Err(refusal(RefusalCode::ValueInvalid, &pointer(path, &key), c));
            }
            *lower = higher.clone();
            sources.retain(|p, _| p != path && !p.starts_with(&prefix));
            mark_sources(s, higher, path, layer, sources);
            return Ok(());
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
                let default = defaults(child);
                if default.is_none()
                    && child.get("x-ck-required") == Some(&Value::Bool(false))
                    && (!value.is_object() || is_opaque(child) || credential(child))
                {
                    // An optional field has no lower value for a bound to compare.
                    mark_sources(child, value, &child_path, layer, sources);
                    target.insert(key.clone(), value.clone());
                    continue;
                }
                let mut initial = default.unwrap_or_else(|| {
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
        mark_sources(s, higher, path, layer, sources);
    }
    Ok(())
}
fn mark_sources(
    s: &Value,
    v: &Value,
    path: &str,
    layer: Layer,
    sources: &mut BTreeMap<String, Layer>,
) {
    if is_opaque(s) {
        sources.insert(path.into(), layer);
        return;
    }
    let s = variant(s, v).unwrap_or(s);
    if let Some(object) = v.as_object() {
        if is_vault_reference(v) {
            sources.insert(path.into(), layer);
        } else {
            for (key, value) in object {
                mark_sources(
                    child_schema(s, key).unwrap_or(&Value::Null),
                    value,
                    &pointer(path, key),
                    layer,
                    sources,
                );
            }
        }
    } else {
        sources.insert(path.into(), layer);
    }
}
fn collect_credentials(s: &Value, value: &Value, out: &mut BTreeSet<Digest>) {
    if is_opaque(s) {
        return;
    }
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
    if is_opaque(s) {
        return Ok(());
    }
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
    if is_opaque(s) {
        return Ok(value.clone());
    }
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
    let opaque = is_opaque(s);
    let Some(node) = s.as_object_mut() else {
        return;
    };
    node.remove("x-ck-options");
    // Walk schema children, not literals such as defaults and enum values.
    if opaque {
        return;
    }
    for key in ["properties", "patternProperties"] {
        if let Some(children) = node.get_mut(key).and_then(Value::as_object_mut) {
            children.values_mut().for_each(strip_options);
        }
    }
    if let Some(items) = node.get_mut("items") {
        strip_options(items);
    }
    if let Some(branches) = node.get_mut("oneOf").and_then(Value::as_array_mut) {
        branches.iter_mut().for_each(strip_options);
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
        if options
            .get("items_from_parent_pointer")
            .is_some_and(|v| !v.as_str().is_some_and(valid_pointer))
        {
            return Err(schema_error(path));
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
    if let Some(order) = node.get("x-ck-ordering").and_then(Value::as_array) {
        if order.len() < 2
            || order
                .iter()
                .any(|v| v.is_null() || v.is_array() || v.is_object())
            || order
                .iter()
                .enumerate()
                .any(|(i, v)| order[..i].iter().any(|x| same_scalar(x, v)))
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
    if is_opaque(s) {
        return s.clone();
    }
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
    fn credential_field() -> Value {
        json!({
            "type":"object",
            "description":"A credential reference",
            "x-ck-kind":"credential_ref",
            "x-ck-apply":"live",
            "x-ck-layers":["user","workspace","project"]
        })
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

    fn proposed_schema(field: Value) -> Result<Schema, Refusal> {
        let mut root = object(json!({"setting": field}));
        root["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        Schema::new(root)
    }

    fn scalar_type_list() -> Value {
        let mut value = field("number", json!(30));
        value["type"] = json!(["string", "number"]);
        value
    }

    fn opaque_field() -> Value {
        json!({
            "description": "A configurable JSON value",
            "default": true,
            "x-ck-kind": "json",
            "x-ck-apply": "live",
            "x-ck-layers": ["user"],
            "oneOf": [
                {"type": "boolean"},
                {"type": "object", "required": ["a"], "properties": {"a": {"type": "number"}}}
            ]
        })
    }

    #[test]
    fn scalar_type_list_replaces_scalar_and_refuses_unlisted_values() {
        let s = proposed_schema(scalar_type_list()).unwrap();
        let result = s
            .review_tuple(
                &layers(json!({"setting":30}), json!({"setting":"1m"})),
                &context(),
            )
            .unwrap();
        assert_eq!(result.effective, json!({"setting":"1m"}));
        assert_eq!(result.sources["/setting"], Layer::Project);
        let error = s
            .validate_layer(Layer::User, &json!({"setting":true}), &context())
            .unwrap_err();
        assert_eq!(error.code, RefusalCode::ValueInvalid);
        assert_eq!(error.path, "/setting");
    }

    #[test]
    fn scalar_type_list_refuses_object_array_and_null_members() {
        let valid = scalar_type_list();
        assert!(proposed_schema(valid.clone()).is_ok());
        for forbidden in ["object", "array", "null"] {
            let mut invalid = valid.clone();
            invalid["type"][0] = json!(forbidden);
            let error = proposed_schema(invalid).unwrap_err();
            assert_eq!(error.code, RefusalCode::SchemaInvalid);
            assert_eq!(error.path, "/setting");
        }
    }

    #[test]
    fn opaque_json_replaces_atomically_and_refuses_non_user_layers() {
        let s = proposed_schema(opaque_field()).unwrap();
        assert_eq!(s.defaults(), json!({"setting":true}));
        let user = LayerValue::new(Layer::User, json!({"setting":{"a":1,"Module Owned!":null}}));
        let result = s
            .review_tuple(std::slice::from_ref(&user), &context())
            .unwrap();
        assert_eq!(result.effective, user.value);
        assert_eq!(
            result.sources,
            BTreeMap::from([("/setting".into(), Layer::User)])
        );
        assert_eq!(s.record(&result.effective).unwrap(), result.effective);
        let mut with_object_default = opaque_field();
        with_object_default["default"] = json!({"a":0,"default_only":true});
        let merged = proposed_schema(with_object_default)
            .unwrap()
            .review_tuple(std::slice::from_ref(&user), &context())
            .unwrap();
        assert_eq!(merged.effective, user.value);
        for layer in [Layer::Workspace, Layer::Project] {
            for value in [json!(false), json!({"a":2}), Value::Null] {
                let error = s
                    .review_tuple(
                        &[
                            user.clone(),
                            LayerValue::new(layer, json!({"setting":value})),
                        ],
                        &context(),
                    )
                    .unwrap_err();
                assert_eq!(error.code, RefusalCode::LayerForbidden);
                assert_eq!(error.path, "/setting");
            }
        }
    }

    #[test]
    fn opaque_json_refuses_invalid_values_and_defaults() {
        let valid = opaque_field();
        let s = proposed_schema(valid.clone()).unwrap();
        for invalid in [json!("true"), json!({}), json!({"a":"1"}), json!(null)] {
            let error = s
                .validate_layer(Layer::User, &json!({"setting":invalid}), &context())
                .unwrap_err();
            assert_eq!(error.code, RefusalCode::ValueInvalid);
            assert_eq!(error.path, "/setting");
        }
        assert_eq!(
            s.validate_layer(
                Layer::User,
                &json!({"setting":{"a":1,"value":"{env:X}"}}),
                &context()
            )
            .unwrap_err()
            .code,
            RefusalCode::ValueInvalid
        );
        let mut invalid_default = valid;
        invalid_default["default"] = json!({"a":false});
        let error = proposed_schema(invalid_default).unwrap_err();
        assert_eq!(error.code, RefusalCode::SchemaInvalid);
        assert_eq!(error.path, "/setting");
    }

    #[test]
    fn opaque_json_refuses_policy_annotations_below_the_leaf() {
        let valid = opaque_field();
        assert!(proposed_schema(valid.clone()).is_ok());
        for (keyword, value) in [
            ("x-ck-kind", json!("credential_ref")),
            ("x-ck-kind", json!("json")),
            ("x-ck-bound", json!("raise_only")),
            ("x-ck-options", json!({})),
            ("x-ck-action", json!({})),
        ] {
            let mut invalid = valid.clone();
            invalid["oneOf"][1]["properties"]["a"][keyword] = value;
            let error = proposed_schema(invalid).unwrap_err();
            assert_eq!(error.code, RefusalCode::SchemaInvalid);
            assert!(error.path.starts_with("/setting/"));
            assert!(error.path.ends_with(keyword));
        }
    }

    #[test]
    fn opaque_json_refuses_unimplemented_assertions() {
        let valid = opaque_field();
        assert!(proposed_schema(valid.clone()).is_ok());
        let mut invalid = valid;
        invalid["oneOf"][1]["dependentRequired"] = json!({"a":["b"]});
        let error = proposed_schema(invalid).unwrap_err();
        assert_eq!(error.code, RefusalCode::SchemaInvalid);
        assert_eq!(error.path, "/setting/oneOf/1/dependentRequired");
    }

    #[test]
    fn opaque_json_accepts_object_type_lists_and_normalizes_layers_to_user_only() {
        let mut field = opaque_field();
        field.as_object_mut().unwrap().remove("oneOf");
        field["type"] = json!(["boolean", "object"]);
        field["x-ck-layers"] = json!(["user", "workspace", "project"]);
        let s = proposed_schema(field.clone()).unwrap();
        assert_eq!(
            s.normalized()["properties"]["setting"]["x-ck-layers"],
            json!(["user"])
        );
        for value in [
            json!(true),
            json!({"Module Key":{"env":{"LANG":"C"}},"more":null}),
            json!({"file":"settings.json"}),
        ] {
            let overlay = json!({"setting":value});
            assert!(s.validate_layer(Layer::User, &overlay, &context()).is_ok());
            assert_eq!(
                s.validate_layer(Layer::Workspace, &overlay, &context())
                    .unwrap_err()
                    .code,
                RefusalCode::LayerForbidden
            );
        }
        field["type"] = json!(["boolean", "null"]);
        let error = proposed_schema(field.clone()).unwrap_err();
        assert_eq!(error.code, RefusalCode::SchemaInvalid);
        assert_eq!(error.path, "/setting/type");
        field["type"] = json!("number");
        field["default"] = json!(1);
        field["x-ck-bound"] = json!("raise_only");
        assert_eq!(proposed_schema(field.clone()).unwrap_err().path, "/setting");
        field["x-ck-layers"] = json!(["user"]);
        let s = proposed_schema(field).unwrap();
        let result = s.review_tuple(
            &[LayerValue::new(Layer::User, json!({"setting":0}))],
            &context(),
        );
        assert_eq!(result.unwrap_err().code, RefusalCode::BoundViolated);
    }

    #[test]
    fn opaque_json_does_not_change_managed_singleton_expansion_object_refusal() {
        let ordinary = proposed_schema(object(json!({"env":field("string", json!("C"))}))).unwrap();
        let error = ordinary
            .validate_layer(Layer::User, &json!({"setting":{"env":"C"}}), &context())
            .unwrap_err();
        assert_eq!(error.code, RefusalCode::ValueInvalid);
        assert_eq!(error.path, "/setting");
    }

    #[test]
    fn opaque_json_enforces_local_ref_siblings_and_object_and_array_constraints() {
        let mut field = opaque_field();
        field.as_object_mut().unwrap().remove("oneOf");
        field["$defs"] = json!({"number_or_null":{"anyOf":[{"type":"number"},{"type":"null"}]}});
        field["type"] = json!("object");
        field["properties"] = json!({
            "Items": {"type":"array", "items":{"$ref":"#/properties/setting/$defs/number_or_null","minimum":0},"minItems":2,"uniqueItems":true}
        });
        field["additionalProperties"] = json!(false);
        field["required"] = json!(["Items"]);
        field["default"] = json!({"Items":[0,null]});
        let s = proposed_schema(field.clone()).unwrap();
        assert_eq!(s.defaults(), json!({"setting":{"Items":[0,null]}}));
        for invalid in [
            json!({"Items":[1]}),
            json!({"Items":[false,1]}),
            json!({"Items":[-1,null]}),
            json!({"Items":[1,1.0]}),
            json!({"Items":[0,null],"more":null}),
        ] {
            let error = s
                .validate_layer(Layer::User, &json!({"setting":invalid}), &context())
                .unwrap_err();
            assert_eq!(error.code, RefusalCode::ValueInvalid);
            assert_eq!(error.path, "/setting");
        }
        field["$defs"]["number_or_null"]["x-ck-kind"] = json!("credential_ref");
        assert!(proposed_schema(field)
            .unwrap_err()
            .path
            .ends_with("/x-ck-kind"));
    }

    #[test]
    fn scalar_type_list_numeric_and_ordered_bounds() {
        let mut field = scalar_type_list();
        field["x-ck-bound"] = json!("raise_only");
        assert_eq!(proposed_schema(field.clone()).unwrap_err().path, "/setting");
        field["enum"] = json!([30, "1m"]);
        field["x-ck-ordering"] = json!([30, "1m"]);
        let s = proposed_schema(field).unwrap();
        assert!(s
            .review_tuple(
                &layers(json!({"setting":30}), json!({"setting":"1m"})),
                &context()
            )
            .is_ok());
        assert_eq!(
            s.review_tuple(
                &layers(json!({"setting":"1m"}), json!({"setting":30})),
                &context()
            )
            .unwrap_err()
            .code,
            RefusalCode::BoundViolated
        );

        let mut numeric = scalar_type_list();
        numeric["type"] = json!(["number", "integer"]);
        numeric["x-ck-bound"] = json!("raise_only");
        let s = proposed_schema(numeric).unwrap();
        assert!(s
            .review_tuple(
                &layers(json!({"setting":30}), json!({"setting":30.5})),
                &context()
            )
            .is_ok());
    }

    #[test]
    fn optional_fields_work_in_maps_and_variants() {
        let mut optional = field("number", json!(0));
        optional.as_object_mut().unwrap().remove("default");
        optional["x-ck-required"] = json!(false);
        let mut a = field("string", json!("a"));
        a["const"] = json!("a");
        let mut b = field("string", json!("b"));
        b["const"] = json!("b");
        let mut first = object(json!({"kind":a,"number":optional}));
        first["required"] = json!(["kind"]);
        let mut second = object(json!({"kind":b}));
        second["required"] = json!(["kind"]);
        let s = schema(json!({
            "map":{"type":"object","unevaluatedProperties":false,"patternProperties":{"^key_":optional}},
            "choice":{"oneOf":[first,second],"default":{"kind":"a"}}
        }));
        assert_eq!(
            s.review_tuple(&[], &context()).unwrap().effective,
            json!({"choice":{"kind":"a"}})
        );
        let overlay = json!({"map":{"key_a":1},"choice":{"number":2}});
        assert_eq!(
            s.review_tuple(&layers(json!({}), overlay), &context())
                .unwrap()
                .effective,
            json!({"map":{"key_a":1},"choice":{"kind":"a","number":2}})
        );
    }

    #[test]
    fn optional_fields_absent_without_defaults_and_inherit_when_not_set() {
        let mut number = field("number", json!(0));
        number.as_object_mut().unwrap().remove("default");
        let error = proposed_schema(number.clone()).unwrap_err();
        assert_eq!(error.code, RefusalCode::SchemaInvalid);
        assert_eq!(error.path, "/setting");
        number["x-ck-required"] = json!(false);
        let s = proposed_schema(number.clone()).unwrap();
        assert_eq!(s.defaults(), json!({}));
        assert_eq!(
            s.review_tuple(&[], &context()).unwrap().effective,
            json!({})
        );
        let user = s
            .review_tuple(&layers(json!({"setting":3}), json!({})), &context())
            .unwrap();
        assert_eq!(user.effective, json!({"setting":3}));
        assert_eq!(user.sources["/setting"], Layer::User);
        let project = s
            .review_tuple(&layers(json!({}), json!({"setting":2})), &context())
            .unwrap();
        assert_eq!(project.effective, json!({"setting":2}));
        assert_eq!(project.sources["/setting"], Layer::Project);
        assert_eq!(
            s.validate_layer(Layer::User, &json!({"setting":null}), &context())
                .unwrap_err()
                .code,
            RefusalCode::ValueInvalid
        );
        number["default"] = json!(1);
        assert_eq!(
            proposed_schema(number).unwrap().defaults(),
            json!({"setting":1})
        );
    }

    #[test]
    fn optional_fields_distinguish_empty_array_and_skip_bounds_without_a_lower_value() {
        let mut list = field("array", json!([]));
        list.as_object_mut().unwrap().remove("default");
        list["x-ck-required"] = json!(false);
        list["x-ck-bound"] = json!("subset_only");
        list["items"] = object(json!({"a":field("number", json!(0))}));
        let s = proposed_schema(list).unwrap();
        assert_eq!(s.defaults(), json!({}));
        let overlay = json!({"setting":[{"a":1}]});
        assert_eq!(
            s.review_tuple(&layers(json!({}), overlay.clone()), &context())
                .unwrap()
                .effective,
            overlay
        );
        assert_eq!(
            s.review_tuple(&layers(json!({"setting":[]}), json!({})), &context())
                .unwrap()
                .effective,
            json!({"setting":[]})
        );
        let error = s
            .review_tuple(&layers(json!({"setting":[]}), overlay), &context())
            .unwrap_err();
        assert_eq!(error.code, RefusalCode::BoundViolated);
        assert_eq!(error.path, "/setting");
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
        let s = schema(json!({"choice":choice,"key":credential_field()}));
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
        let s =
            schema(json!({"key":credential_field(),"endpoint":field("string",json!("approved"))}));
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
    fn policy_contract_optional_credential_has_no_default() {
        let mut key = credential_field();
        key["properties"] = json!({"vault_ref":{"type":"string"}});
        let s = schema(json!({"key":key}));

        let empty = s
            .review_tuple(&[LayerValue::new(Layer::User, json!({}))], &context())
            .unwrap();
        assert_eq!(empty.effective, json!({}));
        assert!(empty.sources.is_empty());

        let user = json!({"key":{"vault_ref":"user-key"}});
        let supplied = s
            .review_tuple(&[LayerValue::new(Layer::User, user.clone())], &context())
            .unwrap();
        assert_eq!(supplied.effective, user);
        assert_eq!(
            supplied.sources,
            BTreeMap::from([("/key".into(), Layer::User)])
        );
    }

    #[test]
    fn policy_contract_credential_defaults_are_refused() {
        let mut declaration = object(json!({"key":credential_field()}));
        declaration["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        let mut direct = declaration.clone();
        direct["properties"]["key"]["default"] = json!({"vault_ref":"example"});
        let error = Schema::new(direct).unwrap_err();
        assert_eq!(error.code, RefusalCode::SchemaInvalid);
        assert_eq!(error.path, "/key");

        declaration["default"] = json!({"key":{"vault_ref":"example"}});
        assert_eq!(
            Schema::new(declaration).unwrap_err().code,
            RefusalCode::SchemaInvalid
        );
    }

    #[test]
    fn policy_contract_other_fields_still_need_a_default_or_required() {
        let mut name = field("string", Value::Null);
        name.as_object_mut().unwrap().remove("default");
        let mut declaration = object(json!({"name":name}));
        declaration["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        let error = Schema::new(declaration).unwrap_err();
        assert_eq!(error.code, RefusalCode::SchemaInvalid);
        assert_eq!(error.path, "/name");
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
    fn policy_contract_first_layer_can_replace_a_union_default() {
        let mut auto_kind = field("string", json!("auto"));
        auto_kind["const"] = json!("auto");
        let mut tokens_kind = field("string", json!("tokens"));
        tokens_kind["const"] = json!("tokens");
        let mut auto = object(json!({
            "kind": auto_kind,
            "floor": field("integer", json!(1000)),
            "ceiling": field("integer", json!(6000))
        }));
        auto["required"] = json!(["kind"]);
        let mut tokens = object(json!({"kind":tokens_kind,"tokens":field("integer",json!(4000))}));
        tokens["required"] = json!(["kind", "tokens"]);
        let s = schema(json!({"protected_tokens":{
            "oneOf":[auto,tokens],
            "default":{"kind":"auto","floor":1000,"ceiling":6000}
        }}));

        let user = json!({"protected_tokens":{"kind":"tokens","tokens":8000}});
        let accepted = s
            .review_tuple(&[LayerValue::new(Layer::User, user.clone())], &context())
            .unwrap();
        assert_eq!(accepted.effective, user);
        assert_eq!(
            accepted.sources,
            BTreeMap::from([
                ("/protected_tokens/kind".into(), Layer::User),
                ("/protected_tokens/tokens".into(), Layer::User),
            ])
        );

        let refusal = s
            .review_tuple(
                &layers(user, json!({"protected_tokens":{"kind":"auto"}})),
                &context(),
            )
            .unwrap_err();
        assert_eq!(refusal.code, RefusalCode::ValueInvalid);
        assert_eq!(refusal.path, "/protected_tokens/kind");

        let partial = json!({"protected_tokens":{"kind":"auto","floor":1200}});
        let merged = s
            .review_tuple(&[LayerValue::new(Layer::User, partial)], &context())
            .unwrap();
        assert_eq!(
            merged.effective,
            json!({"protected_tokens":{"kind":"auto","floor":1200,"ceiling":6000}})
        );
        assert_eq!(merged.sources["/protected_tokens/floor"], Layer::User);
        assert_eq!(merged.sources["/protected_tokens/ceiling"], Layer::Default);
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
        let refusal = s
            .review_tuple(
                &layers(
                    json!({"storage":{"path":"b"}}),
                    json!({"storage":{"kind":"memory","capacity":2}}),
                ),
                &context(),
            )
            .unwrap_err();
        assert_eq!(refusal.code, RefusalCode::ValueInvalid);
        assert_eq!(refusal.path, "/storage/kind");
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

    #[test]
    fn policy_contract_optional_annotation_values_are_checked() {
        for (keyword, value) in [
            ("x-ck-action", json!("not an operation object")),
            ("x-ck-ordering", json!([false, false])),
            ("x-ck-ordering", json!([null, true])),
            ("x-ck-kind", json!("unknown")),
            ("x-ck-required", json!("yes")),
        ] {
            let mut n = field("number", json!(5));
            n[keyword] = value;
            let mut root = object(json!({"n":n}));
            root["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
            assert_eq!(
                Schema::new(root).unwrap_err().code,
                RefusalCode::SchemaInvalid,
                "{keyword}"
            );
        }
    }
}
