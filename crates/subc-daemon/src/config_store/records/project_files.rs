use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use cortexkit_project_identity::EnumerateReply;
use serde::{Deserialize, Serialize};
use subc_config::Digest;

use crate::config_gate::{capture::FileObjectId, project_registry::RegistryFileId, GateError};
use crate::config_store::{StoreBody, StoreError, StoreErrorCode};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryMembership {
    pub projects: BTreeMap<String, Option<String>>,
    pub revision: RegistryMembershipRevision,
    /// An outage is not a legacy user-only install after projects have been seen.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) ever_had_projects: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryMembershipRevision {
    pub generation: i64,
    pub incarnation: String,
}

impl RegistryMembership {
    /// Pass the complete successful `projects.enumerate` reply, before applying
    /// a module's registry scope. A scoped list cannot establish a project drop.
    pub(in crate::config_store) fn from_enumeration(
        reply: &EnumerateReply,
    ) -> Result<Self, GateError> {
        let invalid = || {
            GateError::new(
                "project_registry_unavailable",
                "registry membership enumeration is incomplete or contains duplicate identities",
            )
        };
        let mut workspaces = std::collections::BTreeSet::new();
        for workspace in &reply.workspaces {
            if !workspaces.insert(&workspace.workspace_id) {
                return Err(invalid());
            }
        }
        let mut projects = BTreeMap::new();
        for project in &reply.projects {
            if project
                .workspace_id
                .as_ref()
                .is_some_and(|workspace| !workspaces.contains(workspace))
                || projects
                    .insert(project.project_id.clone(), project.workspace_id.clone())
                    .is_some()
            {
                return Err(invalid());
            }
        }
        Ok(Self {
            ever_had_projects: !projects.is_empty(),
            projects,
            revision: RegistryMembershipRevision {
                generation: reply.generation,
                incarnation: reply.incarnation.clone(),
            },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "layer", rename_all = "snake_case")]
pub enum ProjectFileLayer {
    Workspace { workspace_id: String },
    Project { project_id: String },
}

/// Supply the reviewed registry revision and the file identity of the open
/// workspace or project root. This record is not authority to reopen by path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFileTarget {
    pub module: String,
    #[serde(flatten)]
    pub layer: ProjectFileLayer,
    pub registry_revision: Digest,
    pub root: PathBuf,
    pub root_identity: RegistryFileId,
    pub relative_path: PathBuf,
}

impl ProjectFileTarget {
    pub fn path(&self) -> Option<PathBuf> {
        let relative = Path::new(".cortexkit").join(format!("{}.jsonc", self.module));
        (self.root.is_absolute()
            && self.relative_path == relative
            && self.relative_path.components().count() == 2)
            .then(|| self.root.join(&self.relative_path))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExpectedProjectFile {
    Absent,
    Present {
        identity: FileObjectId,
        digest: Digest,
    },
}

/// Observed state of one module's physical repository file. The two flags
/// are independent: a pending write can coexist with an external proposal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectWriteBackStatus {
    /// A committed edit still has an intent. No matching file was confirmed
    /// by the writer or by a fresh authorized read.
    pub write_back_behind: bool,
    /// The observed digest or presence differs from the approved snapshot.
    /// Only capture approval or reconciliation of matching bytes clears this.
    pub pending_proposal: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ProjectWriteBackIntent {
    pub target: ProjectFileTarget,
    pub expected: ExpectedProjectFile,
    pub generation: u64,
    // A deleted approved file is dirty, not an authorized first publish.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing_approved: bool,
    #[serde(with = "encoded_bytes")]
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for ProjectWriteBackIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectWriteBackIntent")
            .field("target", &self.target)
            .field("expected", &self.expected)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// Include one update per physical file in an approved plan. `Write` carries
/// the reviewed candidate bytes and expected on-disk state. Use `Adopt` only
/// when capture approves that file's current bytes (or absence).
#[derive(Clone)]
pub enum ProjectFileApproval {
    Write {
        target: ProjectFileTarget,
        expected: ExpectedProjectFile,
        bytes: Vec<u8>,
        missing_approved: bool,
    },
    Adopt {
        target: ProjectFileTarget,
        observed_digest: Option<Digest>,
    },
}

impl ProjectFileApproval {
    pub(crate) fn target(&self) -> &ProjectFileTarget {
        match self {
            Self::Write { target, .. } | Self::Adopt { target, .. } => target,
        }
    }

    pub(crate) fn bound_record(&self) -> serde_json::Value {
        match self {
            Self::Write {
                target,
                expected,
                bytes,
                missing_approved,
            } => serde_json::json!({
                "operation": "write", "target": target, "expected": expected,
                "candidate_digest": subc_config::raw_digest(bytes),
                "missing_approved": missing_approved,
            }),
            Self::Adopt {
                target,
                observed_digest,
            } => serde_json::json!({
                "operation": "adopt", "target": target, "candidate_digest": observed_digest,
            }),
        }
    }

    pub(crate) fn into_intent(self, generation: u64) -> Option<ProjectWriteBackIntent> {
        match self {
            Self::Write {
                target,
                expected,
                bytes,
                missing_approved,
            } => Some(ProjectWriteBackIntent {
                target,
                expected,
                generation,
                bytes,
                missing_approved,
            }),
            Self::Adopt { .. } => None,
        }
    }
}

impl StoreBody {
    /// Read the write-back/proposal inputs for a settings row without opening
    /// any roots. A cleared intent with matching observations is in sync; the
    /// store does not retain a separate record of which writes were adopted.
    pub fn project_write_back_status(
        &self,
        module: &str,
        path: &Path,
    ) -> Result<ProjectWriteBackStatus, GateError> {
        use crate::config_gate::{CapturedInput, InputKey};
        use crate::config_store::Snapshot;
        use subc_config::{InputState, RecordedValue};
        let invalid = || GateError::new("config_unavailable", "invalid project file observation");
        let key = serde_json::to_string(&InputKey::ProjectFile {
            module: module.into(),
            path: path.to_string_lossy().into_owned(),
        })
        .map_err(|_| invalid())?;
        let state = |snapshot: &Snapshot| -> Result<InputState, GateError> {
            match snapshot.get(&key) {
                None => Ok(InputState::Absent),
                Some(RecordedValue::Value { value }) => {
                    serde_json::from_value::<CapturedInput>(value.clone())
                        .map(|input| input.state)
                        .map_err(|_| invalid())
                }
                _ => Err(invalid()),
            }
        };
        let pending_proposal = if let Some(observed) = &self.observed {
            state(observed)? != state(&self.approved)?
        } else {
            false
        };
        Ok(ProjectWriteBackStatus {
            write_back_behind: self
                .project_write_back
                .get(path)
                .is_some_and(|intent| intent.target.module == module),
            pending_proposal,
        })
    }

    pub(in crate::config_store) fn apply_project_file_approvals(
        &mut self,
        files: Vec<ProjectFileApproval>,
        generation: u64,
    ) -> Result<(), StoreError> {
        let mut seen = std::collections::BTreeSet::new();
        for file in files {
            let path = file.target().path().ok_or_else(|| {
                StoreError::new(
                    StoreErrorCode::InvalidState,
                    "project write-back target has an invalid layer path",
                )
            })?;
            if !seen.insert(path.clone()) {
                return Err(StoreError::new(
                    StoreErrorCode::InvalidState,
                    "project write-back target occurs more than once",
                ));
            }
            if let Some(intent) = file.into_intent(generation) {
                self.project_write_back.insert(path, intent);
            } else {
                self.project_write_back.remove(&path);
            }
        }
        Ok(())
    }
}

mod encoded_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}
