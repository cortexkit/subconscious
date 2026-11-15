//! Registry-only roots for future project-tuple planning. Runtime delivery never
//! consults this reader; approved tuples must remain usable while it is offline.

use std::{
    collections::BTreeSet,
    fs::File,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use super::{GateError, Result};
use cortexkit_project_identity::{
    Approval, ApprovalState, EnumerateReply, Project, RootIdentity, RootIncarnation, RootRecord,
};
use serde::{Deserialize, Serialize};
use subc_config::Digest;

mod rpc;
pub use rpc::EntorhinalProjectRegistry;

pub const ENUMERATE_OP: &str = "projects.enumerate";

fn unavailable(detail: impl std::fmt::Display) -> GateError {
    GateError::new("project_registry_unavailable", detail)
}

/// The registry publishes the legacy Windows file index, not FILE_ID_INFO's
/// 128-bit id. Keep this comparison identity separate from file capture ids.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum RegistryFileId {
    Unix {
        #[serde(with = "super::capture::decimal_id")]
        device: u64,
        #[serde(with = "super::capture::decimal_id")]
        inode: u64,
    },
    Windows {
        volume_serial: u32,
        #[serde(with = "super::capture::decimal_id")]
        file_index: u64,
    },
}

impl RegistryFileId {
    pub fn parse(text: &str) -> Result<Self> {
        let parts: Vec<_> = text.split(':').collect();
        let invalid = || unavailable("registry fileId is not a supported decimal identity");
        if parts.len() != 3
            || parts[1..]
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit()))
        {
            return Err(invalid());
        }
        match parts[0] {
            "unix" => Ok(Self::Unix {
                device: parts[1].parse().map_err(|_| invalid())?,
                inode: parts[2].parse().map_err(|_| invalid())?,
            }),
            "windows" => Ok(Self::Windows {
                volume_serial: parts[1].parse().map_err(|_| invalid())?,
                file_index: parts[2].parse().map_err(|_| invalid())?,
            }),
            _ => Err(invalid()),
        }
    }

    fn matches(&self, file: &File) -> Result<bool> {
        #[cfg(unix)]
        {
            let id = subc_os::retained_file_identity(file)
                .map_err(|_| unavailable("root identity is unreadable"))?;
            Ok(*self
                == Self::Unix {
                    device: id.device,
                    inode: id.inode,
                })
        }
        #[cfg(windows)]
        {
            let id = subc_os::retained_file_index(file)
                .map_err(|_| unavailable("root identity is unreadable"))?;
            Ok(*self
                == Self::Windows {
                    volume_serial: id.volume_serial,
                    file_index: id.file_index,
                })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRoot {
    pub path: PathBuf,
    pub file_id: Option<RegistryFileId>,
    pub incarnation: Option<RootIncarnation>,
    pub approval: Option<Approval>,
    pub identity: Option<RootIdentity>,
}

impl RegistryRoot {
    fn open(&self, project: &str) -> Result<File> {
        let invalid = || {
            unavailable(format!(
                "{project}: root {} is unavailable or its fileId changed",
                self.path.display()
            ))
        };
        let file = subc_os::open_directory_nofollow_all(&self.path).map_err(|_| invalid())?;
        if self
            .file_id
            .as_ref()
            .map(|id| id.matches(&file))
            .transpose()
            .map_err(|_| invalid())?
            .is_some_and(|matches| !matches)
        {
            return Err(invalid());
        }
        Ok(file)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryProject {
    pub project_id: String,
    pub workspace_id: Option<String>,
    pub project_roots: Vec<RegistryRoot>,
    pub workspace_root: Option<RegistryRoot>,
}

/// Only the affected entries participate in the digest; a global enumeration
/// generation would invalidate reviews after unrelated registry edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRevision {
    pub module: String,
    pub projects: Vec<RegistryProject>,
}

impl RegistryRevision {
    pub fn from_enumerate(
        reply: &EnumerateReply,
        module: &str,
        affected: &BTreeSet<String>,
    ) -> Result<Self> {
        if module.is_empty() || module.contains(['/', '\\']) || module == "." || module == ".." {
            return Err(unavailable("module identity cannot form a layer path"));
        }
        let mut projects = Vec::new();
        for id in affected {
            let mut matches = reply
                .projects
                .iter()
                .filter(|project| project.project_id == *id);
            let project = matches
                .next()
                .ok_or_else(|| unavailable(format!("{id}: project is not registered")))?;
            if matches.next().is_some() {
                return Err(unavailable(format!("{id}: duplicate project identity")));
            }
            let mut project_roots = project
                .root_records
                .iter()
                .flatten()
                .filter(|root| eligible(root))
                .map(|root| -> Result<RegistryRoot> {
                    Ok(RegistryRoot {
                        path: root.root.clone().into(),
                        file_id: parse_id(root.file_id.as_deref(), id)?,
                        incarnation: root.incarnation.clone(),
                        approval: Some(root.approval.clone()),
                        identity: Some(root.identity.clone()),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            project_roots.sort_by(|a, b| a.path.cmp(&b.path));
            if project_roots
                .windows(2)
                .any(|pair| pair[0].path == pair[1].path)
            {
                return Err(unavailable(format!("{id}: duplicate root path")));
            }
            let workspace_root = if let Some(workspace_id) = &project.workspace_id {
                let mut matches = reply
                    .workspaces
                    .iter()
                    .filter(|workspace| workspace.workspace_id == *workspace_id);
                let workspace = matches.next().ok_or_else(|| {
                    unavailable(format!("{id}: workspace {workspace_id} is not registered"))
                })?;
                if matches.next().is_some() {
                    return Err(unavailable(format!("{id}: duplicate workspace identity")));
                }
                workspace
                    .root
                    .as_ref()
                    .map(|path| -> Result<RegistryRoot> {
                        let file_id =
                            parse_id(workspace.file_id.as_deref(), id)?.ok_or_else(|| {
                                unavailable(format!("{id}: workspace root has no fileId"))
                            })?;
                        Ok(RegistryRoot {
                            path: path.into(),
                            file_id: Some(file_id),
                            incarnation: None,
                            approval: None,
                            identity: None,
                        })
                    })
                    .transpose()?
            } else {
                None
            };
            let entry = RegistryProject {
                project_id: id.clone(),
                workspace_id: project.workspace_id.clone(),
                project_roots,
                workspace_root,
            };
            for root in entry
                .project_roots
                .iter()
                .chain(entry.workspace_root.iter())
            {
                root.open(id)?;
            }
            projects.push(entry);
        }
        Ok(Self {
            module: module.into(),
            projects,
        })
    }

    pub fn digest(&self) -> Result<Digest> {
        subc_config::digest(
            subc_config::PURPOSE_SNAPSHOT,
            &serde_json::json!({"kind":"project-registry-v1","registry":self}),
        )
        .map_err(GateError::invalid)
    }

    /// Compare saved affected entries, never a global registry generation.
    pub fn recheck(&self, current: &Self) -> Result<()> {
        if self.module != current.module
            || self.projects.iter().any(|entry| {
                current
                    .projects
                    .iter()
                    .filter(|now| now.project_id == entry.project_id)
                    .collect::<Vec<_>>()
                    != vec![entry]
            })
        {
            return Err(GateError::new(
                "config_changed_since_review",
                "affected project registry entries changed; plan again",
            ));
        }
        Ok(())
    }
}

fn parse_id(text: Option<&str>, project: &str) -> Result<Option<RegistryFileId>> {
    text.map(RegistryFileId::parse)
        .transpose()
        .map_err(|_| unavailable(format!("{project}: invalid registry fileId")))
}

fn layer_path(root: &Path, module: &str) -> PathBuf {
    root.join(".cortexkit").join(format!("{module}.jsonc"))
}

/// Entorhinal's enumerate contract makes every listed root canonical; only one
/// eligible root may contain a given module's project layer.
/// Presence must come from opens beneath retained root handles, supplied by the
/// future tuple planner, never caller-proposed roots or pathname probes.
pub fn project_layer_root<'a>(
    project: &'a Project,
    module: &str,
    present: &BTreeSet<PathBuf>,
) -> Result<Option<&'a RootRecord>> {
    if module.is_empty() || module.contains(['/', '\\']) || module == "." || module == ".." {
        return Err(unavailable("module identity cannot form a layer path"));
    }
    let candidates: Vec<_> = project
        .root_records
        .iter()
        .flatten()
        .filter(|root| {
            eligible(root) && present.contains(&layer_path(Path::new(&root.root), module))
        })
        .collect();
    if candidates.len() > 1 {
        return Err(unavailable(format!(
            "{}: multiple project layer roots: {}",
            project.project_id,
            candidates
                .iter()
                .map(|root| root.root.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(candidates.into_iter().next())
}

fn eligible(root: &RootRecord) -> bool {
    root.approval.state == ApprovalState::Approved
        && matches!(
            root.identity,
            RootIdentity::Bound | RootIdentity::Unverifiable
        )
        && (root.identity == RootIdentity::Bound && root.incarnation.is_some()
            || root.file_id.is_some())
}

pub type RegistryFuture<'a> = Pin<Box<dyn Future<Output = Result<RegistryRevision>> + Send + 'a>>;
pub trait ProjectRegistry: Send + Sync {
    fn snapshot<'a>(
        &'a self,
        module: &'a str,
        affected: &'a BTreeSet<String>,
    ) -> RegistryFuture<'a>;

    fn recheck<'a>(
        &'a self,
        saved: &'a RegistryRevision,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let affected = saved
                .projects
                .iter()
                .map(|project| project.project_id.clone())
                .collect();
            let current = self.snapshot(&saved.module, &affected).await.map_err(|_| {
                GateError::new(
                    "config_changed_since_review",
                    "affected project registry entries are unavailable; plan again",
                )
            })?;
            saved.recheck(&current)
        })
    }
}

#[cfg(test)]
pub(crate) struct OfflineProjectRegistry;
#[cfg(test)]
impl ProjectRegistry for OfflineProjectRegistry {
    fn snapshot<'a>(&'a self, _: &'a str, _: &'a BTreeSet<String>) -> RegistryFuture<'a> {
        Box::pin(async { Err(unavailable("entorhinal is not registered")) })
    }
}

#[cfg(test)]
mod tests;
