//! Shared input validation for workspace and project settings.

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde_json::Value;
use subc_config::{ApplyKind, Layer, LayerValue, ReviewedTuple, Schema, ValidationContext};
use subc_protocol::config::ConfigView;

use super::{capture, review, GateError, Result};

/// Keep one wrapper per declared schema, not per tuple. Credential policy
/// depends only on the normalized schema, including resolved local references.
#[derive(Clone, Debug)]
pub struct ProjectLayerSchema {
    schema: Schema,
    user_only_sections: BTreeSet<String>,
    user_only_patterns: Vec<CredentialPattern>,
}

#[derive(Clone, Debug)]
struct CredentialPattern {
    pattern: Regex,
    named: BTreeSet<String>,
}

/// An input parsed and recorded against its declaration. The parsed value is
/// kept for merging; only the recorded projection is suitable for diagnostics.
pub struct RecordedProjectLayer {
    input: LayerValue,
    pub recorded: Value,
}

impl ProjectLayerSchema {
    pub fn new(schema: Schema) -> Self {
        let mut user_only_sections = BTreeSet::new();
        let mut user_only_patterns = Vec::new();
        collect_sections(
            schema.normalized(),
            &mut user_only_sections,
            &mut user_only_patterns,
        );
        Self {
            schema,
            user_only_sections,
            user_only_patterns,
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Record present workspace/project bytes, whether read from disk or from
    /// approved snapshots. Absent layers are left out, not recorded as `{}`.
    pub fn record_layer(
        &self,
        layer: Layer,
        bytes: &[u8],
        module: &str,
        project: &str,
    ) -> Result<RecordedProjectLayer> {
        if !matches!(layer, Layer::Workspace | Layer::Project) {
            return Err(GateError::new(
                "layer_forbidden",
                "expected a workspace or project layer",
            ));
        }
        let value = capture::parse_layer(bytes)?;
        capture::refuse_expansions(&value, module, "").map_err(|error| {
            // The guard's path can include arbitrary JSON keys from the file.
            GateError::new(
                &error.code,
                format!("{module}/{project}: configuration expansions are not supported"),
            )
        })?;
        let recorded = self.schema.record(&value).map_err(|_| {
            GateError::new(
                "value_invalid",
                format!("{module}/{project}: configuration could not be recorded"),
            )
        })?;
        self.check_credentials(module, project, &value)?;
        Ok(RecordedProjectLayer {
            input: LayerValue::new(layer, value),
            recorded,
        })
    }

    /// Validate ordered layers without deriving any credential destinations.
    pub fn review_tuple(
        &self,
        module: &str,
        project: &str,
        user: Option<Value>,
        layers: &[RecordedProjectLayer],
        base: Option<(&Schema, &ConfigView)>,
    ) -> Result<ReviewedTuple> {
        let mut inputs: Vec<_> = user
            .into_iter()
            .map(|value| LayerValue::new(Layer::User, value))
            .collect();
        for layer in layers {
            self.check_credentials(module, project, &layer.input.value)?;
            inputs.push(layer.input.clone());
        }
        let reviewed = self
            .schema
            .review_tuple(
                &inputs,
                &ValidationContext::new(module, Some(project.into())),
            )
            .map_err(|refusal| {
                GateError::new(
                    capture::refusal_code(refusal.code),
                    format!("{module}/{project}: {}", refusal.path),
                )
            })?;
        refuse_project_apply_kinds(module, project, base, &self.schema, &reviewed)?;
        Ok(reviewed)
    }

    fn check_credentials(&self, module: &str, project: &str, value: &Value) -> Result<()> {
        if let Some(section) = self
            .user_only_sections
            .iter()
            .find(|section| value.get(section.as_str()).is_some())
        {
            return Err(GateError::new(
                "credential_layer_unsupported",
                format!("{module}/{project}: section {section} is user-layer only"),
            ));
        }
        if value.as_object().is_some_and(|fields| {
            fields.keys().any(|key| {
                self.user_only_patterns
                    .iter()
                    .any(|section| !section.named.contains(key) && section.pattern.is_match(key))
            })
        }) {
            return Err(GateError::new(
                "credential_layer_unsupported",
                format!("{module}/{project}: credential sections are user-layer only"),
            ));
        }
        Ok(())
    }
}

fn properties<'a>(schema: &'a Value, kind: &str) -> impl Iterator<Item = (&'a String, &'a Value)> {
    schema
        .get(kind)
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
}

fn branches(schema: &Value) -> impl Iterator<Item = &Value> {
    schema
        .get("oneOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn collect_sections(
    schema: &Value,
    names: &mut BTreeSet<String>,
    patterns: &mut Vec<CredentialPattern>,
) {
    for branch in branches(schema) {
        collect_sections(branch, names, patterns);
    }
    names.extend(
        properties(schema, "properties")
            .filter(|(_, field)| contains_credential(field))
            .map(|(name, _)| name.clone()),
    );
    for (pattern, field) in properties(schema, "patternProperties") {
        if contains_credential(field) {
            // Match using the schema's regex defaults. Exact properties take
            // precedence over pattern matches.
            if let Ok(pattern) = Regex::new(pattern) {
                patterns.push(CredentialPattern {
                    pattern,
                    named: properties(schema, "properties")
                        .map(|(name, _)| name.clone())
                        .collect(),
                });
            }
        }
    }
}

fn contains_credential(schema: &Value) -> bool {
    schema.get("x-ck-kind").and_then(Value::as_str) == Some("credential_ref")
        || ["properties", "patternProperties"]
            .iter()
            .any(|kind| properties(schema, kind).any(|(_, child)| contains_credential(child)))
        || schema.get("items").is_some_and(contains_credential)
        || branches(schema).any(contains_credential)
}

/// Project layers cannot change a module's next supervision settings or
/// rebind its routes. The user tuple still controls both for inherited values.
pub fn refuse_project_apply_kinds(
    module: &str,
    project: &str,
    base: Option<(&Schema, &ConfigView)>,
    candidate_schema: &Schema,
    candidate: &ReviewedTuple,
) -> Result<()> {
    let check = |schema: &Schema, effective: &Value, sources: &BTreeMap<String, Layer>| {
        for path in review::leaf_paths(schema, effective) {
            if matches!(
                review::source(sources, &path),
                Some(Layer::Workspace | Layer::Project)
            ) && matches!(
                review::apply_for_project(schema.normalized(), effective, &path)?,
                ApplyKind::NextSupervision | ApplyKind::RouteRebind
            ) {
                return Err(GateError::new(
                    "project_apply_kind_unsupported",
                    format!("{module}/{project}: {path}"),
                ));
            }
        }
        Ok(())
    };
    if let Some((schema, view)) = base {
        check(schema, &view.effective, &view.sources)?;
    }
    check(candidate_schema, &candidate.effective, &candidate.sources)
}

#[cfg(test)]
mod tests;
