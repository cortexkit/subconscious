use std::{collections::BTreeSet, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::setup::install_journal::VERSION;

use super::{id, paths, Owner, Snapshot};

// Serde normally treats a missing Option field as null. Journal fields that
// authorize changes must explicitly say whether a value is present.
pub(super) fn required_option<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(deserializer)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "record", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Record {
    Begin {
        version: u32,
        id: String,
        began_ms: u64,
        unprotected: bool,
        #[serde(deserialize_with = "required_option")]
        note: Option<String>,
    },
    Step {
        step: Box<Step>,
    },
    Done {
        index: usize,
    },
    RevertStart {
        owner: Owner,
    },
    Undone {
        index: usize,
    },
    Restored {
        targets: Vec<RestoredTarget>,
    },
    RevertEnd,
    Kept,
    Abandoned,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub target: PathBuf,
    pub action: Action,
    pub owner: Owner,
    #[serde(deserialize_with = "required_option")]
    pub module: Option<String>,
    #[serde(deserialize_with = "required_option")]
    pub role: Option<Role>,
    #[serde(deserialize_with = "required_option")]
    pub config_generation: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Binary,
    FormatMap,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Action {
    File {
        #[serde(deserialize_with = "required_option")]
        before: Option<String>,
        #[serde(deserialize_with = "required_option")]
        after: Option<String>,
        #[serde(deserialize_with = "required_option")]
        mode: Option<u32>,
    },
    Bundle {
        #[serde(deserialize_with = "required_option")]
        before: Option<String>,
        after: String,
    },
    Store {
        snapshot_sha256: String,
        snapshot_transfer: SnapshotTransfer,
        floors: Vec<Floor>,
    },
    Activation {
        before: Activation,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotTransfer {
    Moved,
    Copied,
}

impl SnapshotTransfer {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Moved => "snapshot moved",
            Self::Copied => "snapshot copied",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "target", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Activation {
    Daemon {
        #[serde(deserialize_with = "required_option")]
        process: Option<Owner>,
    },
    Module {
        id: String,
        spawn_generation: u64,
        #[serde(deserialize_with = "required_option")]
        process: Option<Owner>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Floor {
    /// Relative to the store's parent and to the saved `floors/` directory.
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredTarget {
    pub target: PathBuf,
    #[serde(deserialize_with = "required_option")]
    pub sha256: Option<String>,
    #[serde(deserialize_with = "required_option")]
    pub mode: Option<u32>,
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::Bundle { .. } => "bundle",
            Self::Store { .. } => "store",
            Self::Activation { .. } => "activation",
        }
    }

    pub fn placement(&self) -> Option<(Option<&str>, Option<&str>)> {
        match self {
            Self::File { before, after, .. } => Some((before.as_deref(), after.as_deref())),
            Self::Bundle { before, after } => Some((before.as_deref(), Some(after))),
            _ => None,
        }
    }
}

impl Record {
    pub(super) fn validate_begin(
        &self,
        expected_id: &str,
    ) -> Result<(u64, Option<String>), String> {
        let Self::Begin {
            version,
            id,
            began_ms,
            unprotected,
            note,
        } = self
        else {
            return Err("missing begin record".into());
        };
        if *version != VERSION {
            return Err(format!(
                "unsupported window journal version {version}; expected {VERSION}"
            ));
        }
        if id != expected_id || !id::valid(id) || !id::matches_begin(id, *began_ms) {
            return Err("begin id or time does not match the window directory".into());
        }
        if !unprotected {
            return Err("begin record does not confirm an unprotected install".into());
        }
        id::validate_note(note.as_deref())?;
        Ok((*began_ms, note.clone()))
    }
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_floor(floor: &Floor) -> bool {
    use std::{ffi::OsStr, path::Component};
    floor
        .path
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
        && floor.path.file_name() == Some(OsStr::new("format-floor.json"))
        && digest(&floor.sha256)
}

impl Step {
    pub(super) fn validate(&self) -> Result<(), String> {
        if !self.owner.valid() || self.role.is_some() && self.module.is_none() {
            return Err("invalid step owner or module role".into());
        }
        let valid = match &self.action {
            Action::File {
                before,
                after,
                mode,
            } => {
                (before.is_some() || after.is_some())
                    && before.iter().chain(after).all(|value| digest(value))
                    && mode.is_none_or(|mode| mode <= 0o7777)
                    && (if cfg!(unix) {
                        before.is_some() == mode.is_some()
                    } else {
                        mode.is_none()
                    })
            }
            Action::Bundle { before, after } => {
                before.iter().all(|value| digest(value)) && digest(after)
            }
            Action::Store {
                snapshot_sha256,
                floors,
                ..
            } => {
                self.module.is_some()
                    && digest(snapshot_sha256)
                    && floors.iter().all(valid_floor)
                    && floors
                        .iter()
                        .map(|floor| &floor.path)
                        .collect::<BTreeSet<_>>()
                        .len()
                        == floors.len()
            }
            Action::Activation { before } => {
                if self.module.is_some() || self.role.is_some() || self.config_generation.is_some()
                {
                    return Err("unexpected placement fields on activation step".into());
                }
                return match before {
                    Activation::Daemon { process }
                        if self.target.as_os_str() == "daemon"
                            && process.as_ref().is_none_or(Owner::valid) =>
                    {
                        Ok(())
                    }
                    Activation::Module { id, process, .. }
                        if super::super::valid_module(id)
                            && id != "daemon"
                            && self.target.as_os_str() == id.as_str()
                            && process.as_ref().is_none_or(Owner::valid) =>
                    {
                        Ok(())
                    }
                    _ => Err("invalid activation target or process identity".into()),
                };
            }
        };
        if !valid
            || !paths::normalized_absolute(&self.target)
            || self
                .module
                .as_ref()
                .is_some_and(|module| !super::super::valid_module(module))
        {
            return Err("invalid placement target or step data".into());
        }
        Ok(())
    }
}

pub(super) fn validate_restored(
    snapshot: &Snapshot,
    targets: &[RestoredTarget],
) -> Result<(), String> {
    let mut expected: Vec<(&std::path::Path, Option<&str>)> = Vec::new();
    for step in &snapshot.steps {
        let Some((before, _)) = step.step.action.placement() else {
            continue;
        };
        if expected
            .iter()
            .any(|(path, _)| paths::same_path(path, &step.step.target))
        {
            continue;
        }
        expected.push((step.step.target.as_path(), before));
    }
    if targets.len() != expected.len()
        || expected.iter().any(|(path, before)| {
            targets
                .iter()
                .filter(|target| {
                    &target.target == path
                        && target.sha256.as_deref() == *before
                        && target.mode.is_none_or(|mode| mode <= 0o7777)
                        && (target.mode.is_none() || before.is_some())
                })
                .count()
                != 1
        })
    {
        return Err("restored targets do not match the recorded placements".into());
    }
    Ok(())
}
