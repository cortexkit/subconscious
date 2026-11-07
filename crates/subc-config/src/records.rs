use crate::{digest, CanonicalError, Digest, PURPOSE_PLAN, PURPOSE_VALUE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Layer {
    Default,
    User,
    Workspace,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApplyKind {
    Live,
    ModuleRestart,
    DaemonRestart,
    NextSupervision,
    RouteRebind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Bound {
    RaiseOnly,
    LowerOnly,
    SubsetOnly,
}

/// Serializable refusals deliberately contain no submitted values or schema text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RefusalCode {
    SchemaInvalid,
    LayerForbidden,
    ValueInvalid,
    BoundViolated,
    CredentialShared,
    PlanStale,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Refusal {
    pub code: RefusalCode,
    pub path: String,
    pub project_id: Option<String>,
}
impl Refusal {
    pub fn new(code: RefusalCode, path: impl Into<String>, project_id: Option<String>) -> Self {
        Self {
            code,
            path: path.into(),
            project_id,
        }
    }
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} at {} ({:?})",
            self.code, self.path, self.project_id
        )
    }
}
impl std::error::Error for Refusal {}

/// A rejected literal and an accepted vault reference use the same redacted
/// record. Neither the opaque reference id nor a literal is retained in views.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecordedValue {
    Value { value: Value },
    Redacted { digest: Digest, redacted: bool },
}
impl RecordedValue {
    pub fn new(value: Value) -> Self {
        Self::Value { value }
    }
    pub fn redacted(value: &Value) -> Result<Self, CanonicalError> {
        Ok(Self::Redacted {
            digest: digest(PURPOSE_VALUE, value)?,
            redacted: true,
        })
    }
}

/// Missing files are inputs, distinct from the digest of an empty document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum InputState {
    Absent,
    Present { digest: Digest },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct InputVector {
    pub inputs: BTreeMap<String, InputState>,
}
impl InputVector {
    pub fn new(inputs: BTreeMap<String, InputState>) -> Self {
        Self { inputs }
    }
    /// Compare the saved read set only. Unrelated writes do not stale a plan.
    pub fn matches(&self, current: &Self) -> bool {
        self.inputs
            .iter()
            .all(|(key, state)| current.inputs.get(key) == Some(state))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FieldChange {
    pub module_id: String,
    pub project_id: Option<String>,
    pub path: String,
    pub old: RecordedValue,
    pub new: RecordedValue,
    pub old_source: Layer,
    pub new_source: Layer,
    pub apply: ApplyKind,
    pub reasons: Vec<String>,
}
impl FieldChange {
    pub fn new(
        module_id: impl Into<String>,
        path: impl Into<String>,
        old: RecordedValue,
        new: RecordedValue,
        apply: ApplyKind,
    ) -> Self {
        Self {
            module_id: module_id.into(),
            path: path.into(),
            project_id: None,
            old,
            new,
            old_source: Layer::Default,
            new_source: Layer::Default,
            apply,
            reasons: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Changeset {
    pub fields: Vec<FieldChange>,
    pub affected_projects: Vec<String>,
    pub policy_changes: Vec<Value>,
    pub launch_manifests: BTreeMap<String, Value>,
    pub apply_plan: Value,
}
impl Changeset {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Immutable approval payload. The input vector includes subc.jsonc and every
/// touched module's layers, declaration and covered program identities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Plan {
    pub base_revision: u64,
    pub base_generation: u64,
    pub active_generations: BTreeMap<String, u64>,
    pub daemon_incarnation: String,
    pub input_vector: InputVector,
    pub changeset: Changeset,
}
impl Plan {
    pub fn new(
        base_revision: u64,
        base_generation: u64,
        daemon_incarnation: impl Into<String>,
        input_vector: InputVector,
        changeset: Changeset,
    ) -> Self {
        Self {
            base_revision,
            base_generation,
            daemon_incarnation: daemon_incarnation.into(),
            active_generations: BTreeMap::new(),
            input_vector,
            changeset,
        }
    }
    pub fn digest(&self) -> Result<Digest, CanonicalError> {
        digest(
            PURPOSE_PLAN,
            &serde_json::to_value(self).map_err(|_| CanonicalError::Serialization)?,
        )
    }
    pub fn check_fresh(&self, base_revision: u64, current: &InputVector) -> Result<(), Refusal> {
        if self.base_revision != base_revision || !self.input_vector.matches(current) {
            return Err(Refusal::new(RefusalCode::PlanStale, "", None));
        }
        Ok(())
    }
}

/// Requests are strict; stored records and replies tolerate added fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ApplyRequest {
    pub digest: Digest,
}
impl ApplyRequest {
    pub fn new(digest: Digest) -> Self {
        Self { digest }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GenerationState {
    Planned,
    Approved,
    Pending,
    Warming,
    Active,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Generation {
    pub id: u64,
    pub plan_digest: Digest,
    pub input_vector: InputVector,
    pub declaration_digests: BTreeMap<String, Digest>,
    pub module_states: BTreeMap<String, GenerationState>,
    pub active_generations: BTreeMap<String, u64>,
    pub never_redeliver: Vec<String>,
    pub development: bool,
    pub changeset: Changeset,
}
impl Generation {
    pub fn new(
        id: u64,
        plan_digest: Digest,
        input_vector: InputVector,
        changeset: Changeset,
    ) -> Self {
        Self {
            id,
            plan_digest,
            input_vector,
            changeset,
            declaration_digests: BTreeMap::new(),
            module_states: BTreeMap::new(),
            active_generations: BTreeMap::new(),
            never_redeliver: Vec::new(),
            development: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn records_contract_plan_binding_and_evolution() {
        let d = digest(PURPOSE_VALUE, &json!(1)).unwrap();
        let input = InputVector::new(BTreeMap::from([
            ("subc.jsonc".into(), InputState::Absent),
            ("project".into(), InputState::Present { digest: d }),
        ]));
        let plan = Plan::new(7, 2, "incarnation", input.clone(), Changeset::new());
        let mut current = input.clone();
        current
            .inputs
            .insert("unrelated".into(), InputState::Absent);
        assert!(plan.check_fresh(7, &current).is_ok());
        current.inputs.insert("project".into(), InputState::Absent);
        assert_eq!(
            plan.check_fresh(7, &current).unwrap_err().code,
            RefusalCode::PlanStale
        );
        assert!(plan.check_fresh(8, &input).is_err());
        let mut changed = plan.clone();
        changed.base_revision += 1;
        assert_ne!(plan.digest().unwrap(), changed.digest().unwrap());
        let mut wire = serde_json::to_value(&plan).unwrap();
        wire["future"] = json!(true);
        assert!(serde_json::from_value::<Plan>(wire).is_ok());
        let request = json!({"digest":plan.digest().unwrap(), "future":true});
        assert!(serde_json::from_value::<ApplyRequest>(request).is_err());
    }
}
