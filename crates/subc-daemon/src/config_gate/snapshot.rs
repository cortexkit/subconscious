use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use subc_config::{InputState, InputVector, RecordedValue};

use super::{GateError, Result};
use crate::config_store::Snapshot;

/// Semantic roles are supplied by the daemon's file capturer, never by a client.
/// Each module entry is also retained separately from the whole daemon document
/// so launch-policy changes participate in that module's read set.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum InputKey {
    Daemon,
    Entry(String),
    Layer { module: String, path: String },
    Declaration(String),
    Program { module: String, path: String },
}

impl InputKey {
    pub fn module(&self) -> Option<&str> {
        match self {
            Self::Daemon => None,
            Self::Entry(module) | Self::Declaration(module) => Some(module),
            Self::Layer { module, .. } | Self::Program { module, .. } => Some(module),
        }
    }
    pub(super) fn is_program(&self) -> bool {
        matches!(self, Self::Program { .. })
    }
}

/// `state` binds raw bytes AND retained file identity, including an absent-file
/// marker. `value` is the parsed, secret-redacted view used in plans and history.
/// This diagnostic capture never contains a credential or vault reference in
/// plaintext. Separate approved delivery bytes may contain reference strings;
/// literal secret values are refused before a plan or generation is retained.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapturedInput {
    pub state: InputState,
    pub value: RecordedValue,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GateSnapshot {
    pub inputs: BTreeMap<InputKey, CapturedInput>,
}

impl GateSnapshot {
    pub fn to_store(&self) -> Result<Snapshot> {
        self.inputs
            .iter()
            .map(|(key, input)| {
                Ok((
                    serde_json::to_string(key).map_err(GateError::invalid)?,
                    RecordedValue::new(serde_json::to_value(input).map_err(GateError::invalid)?),
                ))
            })
            .collect()
    }

    pub fn from_store(snapshot: &Snapshot) -> Result<Self> {
        let inputs = snapshot
            .iter()
            .map(|(key, value)| {
                let RecordedValue::Value { value } = value else {
                    return Err(GateError::invalid("invalid gate snapshot envelope"));
                };
                Ok((
                    serde_json::from_str(key).map_err(GateError::invalid)?,
                    serde_json::from_value(value.clone()).map_err(GateError::invalid)?,
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Self { inputs })
    }

    pub(super) fn modules(&self) -> BTreeSet<String> {
        self.inputs
            .keys()
            .filter_map(|key| key.module().map(str::to_owned))
            .collect()
    }

    pub(super) fn changed_keys<'a>(&'a self, other: &'a Self) -> BTreeSet<&'a InputKey> {
        self.inputs
            .keys()
            .chain(other.inputs.keys())
            .filter(|key| self.inputs.get(key) != other.inputs.get(key))
            .collect()
    }

    /// Aggregate the whole module inventory, not just paths that changed. This
    /// detects newly seen project layers and replaced covered files as well as
    /// edits to an unchanged project layer used by a bound check.
    pub fn read_set(&self, modules: &BTreeSet<String>) -> Result<InputVector> {
        let daemon = self
            .inputs
            .get(&InputKey::Daemon)
            .ok_or_else(|| GateError::invalid("snapshot has no subc.jsonc input"))?;
        let mut inputs = BTreeMap::from([("subc.jsonc".into(), daemon.state.clone())]);
        for module in modules {
            let inventory: BTreeMap<_, _> = self
                .inputs
                .iter()
                .filter(|(key, _)| key.module() == Some(module))
                .map(|(key, value)| (serde_json::to_string(key).expect("input key"), value))
                .collect();
            inputs.insert(
                format!(
                    "module:{}",
                    serde_json::to_string(module).expect("module id")
                ),
                InputState::Present {
                    digest: subc_config::raw_digest(
                        &serde_json::to_vec(&inventory).map_err(GateError::invalid)?,
                    ),
                },
            );
        }
        Ok(InputVector::new(inputs))
    }

    pub(super) fn restore_programs_from(&mut self, current: &Self) -> Result<()> {
        for (key, identity) in self.inputs.iter_mut().filter(|(key, _)| key.is_program()) {
            *identity = current.inputs.get(key).cloned().ok_or_else(|| {
                GateError::new(
                    "program_missing",
                    "restored program path has no current captured identity",
                )
            })?;
        }
        Ok(())
    }
}
