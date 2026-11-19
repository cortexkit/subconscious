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

#[derive(Clone, Serialize, Deserialize)]
pub struct ProjectWriteBackIntent {
    pub target: ProjectFileTarget,
    pub expected: ExpectedProjectFile,
    pub generation: u64,
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
            } => serde_json::json!({
                "operation": "write", "target": target, "expected": expected,
                "candidate_digest": subc_config::raw_digest(bytes),
            }),
            Self::Adopt {
                target,
                observed_digest,
            } => serde_json::json!({
                "operation": "adopt", "target": target, "candidate_digest": observed_digest,
            }),
        }
    }

    pub(in crate::config_store) fn into_intent(
        self,
        generation: u64,
    ) -> Option<ProjectWriteBackIntent> {
        match self {
            Self::Write {
                target,
                expected,
                bytes,
            } => Some(ProjectWriteBackIntent {
                target,
                expected,
                generation,
                bytes,
            }),
            Self::Adopt { .. } => None,
        }
    }
}

impl StoreBody {
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
