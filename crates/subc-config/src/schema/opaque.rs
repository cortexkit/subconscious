//! Assertions on opaque JSON are separate from the managed settings vocabulary.

use super::{matches_type, pointer, same_scalar, schema_error, schema_regex, valid_pointer};
use crate::Refusal;
use serde_json::Value;

pub(super) fn normalize(
    root: &Value,
    schema: &mut Value,
    stack: &mut Vec<String>,
    path: &str,
    leaf: bool,
) -> Result<(), Refusal> {
    if schema.is_boolean() {
        return Ok(());
    }
    let node = schema.as_object_mut().ok_or_else(|| schema_error(path))?;
    for (key, value) in node.iter_mut() {
        let here = pointer(path, key);
        let bad = || schema_error(&here);
        match key.as_str() {
            "x-ck-kind" | "x-ck-bound" | "x-ck-options" | "x-ck-action" if !leaf => {
                return Err(bad());
            }
            "x-ck-kind" | "x-ck-bound" | "x-ck-options" | "x-ck-action" | "x-ck-layers"
            | "x-ck-apply" | "x-ck-required" | "x-ck-ordering" | "x-ck-setup" | "x-ck-section"
            | "x-ck-order" | "default" | "const" => {}
            "description" | "title" | "$comment" if value.is_string() => {}
            "deprecated" | "readOnly" | "writeOnly" if value.is_boolean() => {}
            "examples" if value.is_array() => {}
            "type" if valid_types(value, leaf) => {}
            "enum" if value.as_array().is_some_and(|values| !values.is_empty()) => {}
            "minLength" | "maxLength" | "minProperties" | "maxProperties" | "minItems"
            | "maxItems"
                if value.as_u64().is_some() => {}
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
                if value.is_number() => {}
            "uniqueItems" if value.is_boolean() => {}
            "pattern" if value.as_str().is_some_and(|p| schema_regex(p).is_some()) => {}
            "required"
                if value.as_array().is_some_and(|keys| {
                    keys.iter()
                        .enumerate()
                        .all(|(i, key)| key.is_string() && !keys[..i].contains(key))
                }) => {}
            "$ref" if value.is_string() => {}
            "properties" | "patternProperties" | "$defs" => {
                for (name, child) in value.as_object_mut().ok_or_else(bad)? {
                    let child_path = pointer(&here, name);
                    if key == "patternProperties" && schema_regex(name).is_none() {
                        return Err(schema_error(&child_path));
                    }
                    normalize(root, child, stack, &child_path, false)?;
                }
            }
            "additionalProperties" | "propertyNames" | "items" | "not" => {
                normalize(root, value, stack, &here, false)?;
            }
            "oneOf" | "anyOf" | "allOf" | "prefixItems" => {
                let children = value
                    .as_array_mut()
                    .filter(|a| !a.is_empty())
                    .ok_or_else(bad)?;
                for (i, child) in children.iter_mut().enumerate() {
                    normalize(root, child, stack, &pointer(&here, &i.to_string()), false)?;
                }
            }
            _ => return Err(bad()),
        }
    }
    node.remove("$defs");
    if let Some(reference) = node.remove("$ref") {
        let here = pointer(path, "$ref");
        let reference = reference.as_str().expect("checked reference string");
        let target = reference
            .strip_prefix('#')
            .filter(|p| p.starts_with('/') && valid_pointer(p))
            .filter(|_| !stack.iter().any(|r| r == reference))
            .and_then(|p| root.pointer(p))
            .ok_or_else(|| schema_error(&here))?;
        stack.push(reference.to_owned());
        let mut target = target.clone();
        let result = normalize(root, &mut target, stack, &here, false);
        stack.pop();
        result?;
        // A reference and its siblings are simultaneous assertions in draft 2020-12.
        node.entry("allOf")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("checked allOf array")
            .push(target);
    }
    Ok(())
}

fn valid_types(value: &Value, leaf: bool) -> bool {
    let valid = |v: &Value| {
        matches!(
            v.as_str(),
            Some("boolean" | "integer" | "number" | "string" | "object" | "array")
        ) || (!leaf && v.as_str() == Some("null"))
    };
    valid(value)
        || value.as_array().is_some_and(|types| {
            !types.is_empty()
                && types
                    .iter()
                    .enumerate()
                    .all(|(i, kind)| valid(kind) && !types[..i].contains(kind))
        })
}

pub(super) fn matches(s: &Value, value: &Value) -> bool {
    if let Some(always) = s.as_bool() {
        return always;
    }
    if s.get("type").is_some_and(|kind| !matches_type(kind, value))
        || s.get("const").is_some_and(|v| !same_scalar(v, value))
        || s.get("enum")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.iter().any(|v| same_scalar(v, value)))
        || s.get("not").is_some_and(|not| matches(not, value))
    {
        return false;
    }
    for (key, min, max) in [
        ("allOf", 0, usize::MAX),
        ("anyOf", 1, usize::MAX),
        ("oneOf", 1, 1),
    ] {
        if let Some(branches) = s.get(key).and_then(Value::as_array) {
            let count = branches
                .iter()
                .filter(|branch| matches(branch, value))
                .count();
            if count < min || count > max || (key == "allOf" && count != branches.len()) {
                return false;
            }
        }
    }
    if let Some(text) = value.as_str() {
        if !length(s, text.chars().count(), "minLength", "maxLength")
            || s.get("pattern")
                .and_then(Value::as_str)
                .is_some_and(|p| !regex(p).is_match(text))
        {
            return false;
        }
    }
    if let Some(n) = value.as_f64() {
        if [
            ("minimum", n, false),
            ("exclusiveMinimum", n, true),
            ("maximum", -n, false),
            ("exclusiveMaximum", -n, true),
        ]
        .iter()
        .any(|(key, a, strict)| {
            s.get(*key).and_then(Value::as_f64).is_some_and(|bound| {
                let b = if key.ends_with("Maximum") || *key == "maximum" {
                    -bound
                } else {
                    bound
                };
                *a < b || (*strict && *a == b)
            })
        }) {
            return false;
        }
    }
    if let Some(object) = value.as_object() {
        if !length(s, object.len(), "minProperties", "maxProperties")
            || s.get("required")
                .and_then(Value::as_array)
                .is_some_and(|keys| {
                    keys.iter().any(|key| {
                        !object.contains_key(key.as_str().expect("checked property name"))
                    })
                })
        {
            return false;
        }
        let patterns: Vec<_> = s
            .get("patternProperties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(pattern, schema)| (regex(pattern), schema))
            .collect();
        for (key, child) in object {
            if s.get("propertyNames")
                .is_some_and(|schema| !matches(schema, &Value::String(key.clone())))
            {
                return false;
            }
            let exact = s.get("properties").and_then(|p| p.get(key));
            if exact.is_some_and(|schema| !matches(schema, child)) {
                return false;
            }
            let mut selected = exact.is_some();
            for (pattern, schema) in &patterns {
                if pattern.is_match(key) {
                    selected = true;
                    if !matches(schema, child) {
                        return false;
                    }
                }
            }
            if !selected
                && s.get("additionalProperties")
                    .is_some_and(|schema| !matches(schema, child))
            {
                return false;
            }
        }
    }
    if let Some(array) = value.as_array() {
        if !length(s, array.len(), "minItems", "maxItems")
            || (s.get("uniqueItems") == Some(&Value::Bool(true))
                && array
                    .iter()
                    .enumerate()
                    .any(|(i, v)| array[..i].iter().any(|x| same_scalar(x, v))))
        {
            return false;
        }
        for (i, child) in array.iter().enumerate() {
            let schema = s
                .get("prefixItems")
                .and_then(|a| a.get(i))
                .or_else(|| s.get("items"));
            if schema.is_some_and(|schema| !matches(schema, child)) {
                return false;
            }
        }
    }
    true
}

fn regex(pattern: &str) -> std::sync::Arc<regex::Regex> {
    schema_regex(pattern).expect("checked opaque schema pattern")
}

fn length(s: &Value, n: usize, min: &str, max: &str) -> bool {
    s.get(min)
        .and_then(Value::as_u64)
        .is_none_or(|v| n as u64 >= v)
        && s.get(max)
            .and_then(Value::as_u64)
            .is_none_or(|v| n as u64 <= v)
}
