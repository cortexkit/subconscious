//! Registry authority and retained-root captures for project settings.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

pub use super::root_io::RootIoPool;
use super::{root_io::RootIoError, GateError, Result};
use cortexkit_project_identity::{
    Approval, ApprovalState, EnumerateReply, Project, RootIdentity, RootIncarnation, RootRecord,
};
use serde::{Deserialize, Serialize};
use subc_config::Digest;

mod layers;
mod rpc;
use layers::{open_root, OpenedRoot};
pub use layers::{LayerFile, LayerLocation};
pub use rpc::EntorhinalProjectRegistry;

pub const ENUMERATE_OP: &str = "projects.enumerate";

fn unavailable(detail: impl std::fmt::Display) -> GateError {
    GateError::new("project_registry_unavailable", detail)
}

fn layer_unavailable(detail: impl std::fmt::Display) -> GateError {
    GateError::new("config_unavailable", detail)
}

fn changed() -> GateError {
    GateError::new(
        "config_changed_since_review",
        "affected project registry entries or layer locations changed; plan again",
    )
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

    fn read(file: &File) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let id = subc_os::retained_file_identity(file)?;
            Ok(Self::Unix {
                device: id.device,
                inode: id.inode,
            })
        }
        #[cfg(windows)]
        {
            let id = subc_os::retained_file_index(file)?;
            Ok(Self::Windows {
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

#[derive(Clone, Debug)]
enum RootCheckFailure {
    Pool(RootIoError),
    InvalidPath,
    OpenRefused(Arc<subc_os::OpenRegularError>),
    IdentityUnreadable(Arc<std::io::Error>),
    IdentityMismatch,
}

impl std::fmt::Display for RootCheckFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pool(error) => std::fmt::Display::fmt(&GateError::from(*error), f),
            Self::InvalidPath => f.write_str("root must be a canonical absolute path"),
            Self::OpenRefused(error) => write!(f, "root open refused: {error}"),
            Self::IdentityUnreadable(error) => write!(f, "root identity is unreadable: {error}"),
            Self::IdentityMismatch => f.write_str("root fileId changed"),
        }
    }
}

impl RootCheckFailure {
    fn error(&self, project: &str, path: &Path, rechecking: bool) -> GateError {
        use subc_os::OpenRegularError;
        let code = match self {
            Self::InvalidPath => "layer_root_invalid",
            Self::OpenRefused(error) => match error.as_ref() {
                OpenRegularError::Symlink => "layer_symlink",
                OpenRegularError::NotRegularFile => "layer_not_regular",
                OpenRegularError::Io(error)
                    if error.kind() == std::io::ErrorKind::NotFound && rechecking =>
                {
                    return changed()
                }
                OpenRegularError::Io(_) => "config_unavailable",
            },
            Self::IdentityMismatch if rechecking => return changed(),
            _ => "config_unavailable",
        };
        if rechecking && code.starts_with("layer_") {
            return changed();
        }
        GateError::new(code, format!("{project}: root {}: {self}", path.display()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryProject {
    pub project_id: String,
    pub workspace_id: Option<String>,
    pub project_roots: Vec<RegistryRoot>,
    pub workspace_root: Option<RegistryRoot>,
}

impl RegistryProject {
    pub(super) fn roots(&self) -> impl Iterator<Item = &RegistryRoot> {
        self.project_roots.iter().chain(&self.workspace_root)
    }
}

/// Select explicit project IDs and every reader of the named paths or workspaces.
#[derive(Clone, Debug, Default)]
pub struct RegistryScope {
    pub projects: BTreeSet<String>,
    pub paths: BTreeSet<PathBuf>,
    pub workspaces: BTreeSet<String>,
    all_registered: bool,
    capture: CaptureLayers,
}

#[derive(Clone, Copy, Debug, Default)]
enum CaptureLayers {
    #[default]
    All,
    Targets,
}

impl RegistryScope {
    pub fn new(
        projects: BTreeSet<String>,
        paths: BTreeSet<PathBuf>,
        workspaces: BTreeSet<String>,
    ) -> Self {
        Self {
            projects,
            paths,
            workspaces,
            ..Self::default()
        }
    }

    /// Scope a capture to all currently registered projects (including rootless projects).
    pub fn all() -> Self {
        Self {
            all_registered: true,
            ..Self::default()
        }
    }

    pub(super) fn include_all(&mut self) {
        self.all_registered = true;
    }

    fn includes(
        &self,
        id: &str,
        workspace: Option<&String>,
        roots: impl IntoIterator<Item = impl AsRef<Path>>,
        module: &str,
    ) -> bool {
        self.all_registered
            || self.projects.contains(id)
            || workspace.is_some_and(|id| self.workspaces.contains(id))
            || roots
                .into_iter()
                .any(|root| self.paths.contains(&layer_path(root.as_ref(), module)))
    }
}

/// Inputs of one tuple, after assigning a same-root layer to the project only.
pub struct ProjectLayers<'a> {
    pub workspace: Option<(&'a Path, &'a LayerFile)>,
    pub project: Option<(&'a Path, &'a LayerFile)>,
}

/// Only the affected entries participate in the digest; a global enumeration
/// generation would invalidate reviews after unrelated registry edits.
#[derive(Clone, Serialize, Deserialize)]
pub struct RegistryRevision {
    pub module: String,
    pub projects: Vec<RegistryProject>,
    #[serde(skip)]
    opened: Arc<BTreeMap<PathBuf, std::result::Result<OpenedRoot, RootCheckFailure>>>,
    #[serde(skip)]
    metadata_errors: BTreeMap<String, GateError>,
}

impl std::fmt::Debug for RegistryRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryRevision")
            .field("module", &self.module)
            .field("projects", &self.projects)
            .finish_non_exhaustive()
    }
}

impl PartialEq for RegistryRevision {
    fn eq(&self, other: &Self) -> bool {
        self.module == other.module && self.projects == other.projects
    }
}
impl Eq for RegistryRevision {}

impl RegistryRevision {
    pub async fn from_enumerate(
        pool: &RootIoPool,
        reply: &EnumerateReply,
        module: &str,
        affected: &BTreeSet<String>,
    ) -> Result<Self> {
        let mut revision = Self::parse_enumeration(reply, module, affected)?;
        revision
            .capture_roots(pool, &RegistryScope::default())
            .await;
        Ok(revision)
    }

    pub async fn from_scope(
        pool: &RootIoPool,
        reply: &EnumerateReply,
        module: &str,
        scope: &RegistryScope,
    ) -> Result<Self> {
        let affected = reply
            .projects
            .iter()
            .filter_map(|project| {
                let roots = project
                    .root_records
                    .iter()
                    .flatten()
                    .filter(|root| eligible(root))
                    .map(|root| root.root.as_str());
                let workspace = reply
                    .workspaces
                    .iter()
                    .filter(|workspace| {
                        project.workspace_id.as_ref() == Some(&workspace.workspace_id)
                    })
                    .filter_map(|workspace| workspace.root.as_deref());
                scope
                    .includes(
                        &project.project_id,
                        project.workspace_id.as_ref(),
                        roots.chain(workspace),
                        module,
                    )
                    .then(|| project.project_id.clone())
            })
            .collect();
        let mut revision = Self::parse_enumeration(reply, module, &affected)?;
        revision.capture_roots(pool, scope).await;
        Ok(revision)
    }

    fn parse_enumeration(
        reply: &EnumerateReply,
        module: &str,
        affected: &BTreeSet<String>,
    ) -> Result<Self> {
        validate_module(module)?;
        let mut projects = Vec::new();
        let mut metadata_errors = BTreeMap::new();
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
            let read_id =
                |text, path, errors: &mut BTreeMap<String, GateError>| match parse_id(text) {
                    Ok(file_id) => file_id,
                    Err(_) => {
                        errors.entry(id.clone()).or_insert_with(|| {
                            layer_unavailable(format!("{id}: root {path}: invalid registry fileId"))
                        });
                        None
                    }
                };
            let mut project_roots = project
                .root_records
                .iter()
                .flatten()
                .filter(|root| eligible(root))
                .map(|root| RegistryRoot {
                    path: root.root.clone().into(),
                    file_id: read_id(
                        root.file_id.as_deref(),
                        root.root.as_str(),
                        &mut metadata_errors,
                    ),
                    incarnation: root.incarnation.clone(),
                    approval: Some(root.approval.clone()),
                    identity: Some(root.identity.clone()),
                })
                .collect::<Vec<_>>();
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
                workspace.root.as_ref().map(|path| {
                    let file_id = read_id(workspace.file_id.as_deref(), path, &mut metadata_errors);
                    if workspace.file_id.is_none() {
                        metadata_errors.entry(id.clone()).or_insert_with(|| {
                            layer_unavailable(format!("{id}: workspace root {path} has no fileId"))
                        });
                    }
                    RegistryRoot {
                        path: path.into(),
                        file_id,
                        incarnation: None,
                        approval: None,
                        identity: None,
                    }
                })
            } else {
                None
            };
            projects.push(RegistryProject {
                project_id: id.clone(),
                workspace_id: project.workspace_id.clone(),
                project_roots,
                workspace_root,
            });
        }
        Ok(Self {
            module: module.into(),
            projects,
            opened: Arc::default(),
            metadata_errors,
        })
    }

    async fn capture_roots(&mut self, pool: &RootIoPool, scope: &RegistryScope) {
        // Open and read once per physical path, regardless of the layer role.
        let mut paths = BTreeMap::<_, Vec<_>>::new();
        for project in &self.projects {
            for root in project.roots() {
                let allowed = paths.entry(root.path.clone()).or_default();
                if !self.metadata_errors.contains_key(&project.project_id) {
                    allowed.push(root.file_id.clone());
                }
            }
        }
        let mut probes = paths.into_iter();
        let leaf = std::ffi::OsString::from(format!("{}.jsonc", self.module));
        let mut observed = BTreeMap::new();
        loop {
            let jobs: Vec<_> = probes
                .by_ref()
                .take(pool.batch_slots())
                .map(|(root, allowed)| {
                    let path = root.clone();
                    let leaf = (matches!(scope.capture, CaptureLayers::All)
                        || scope.paths.contains(&layer_path(&path, &self.module)))
                    .then(|| leaf.clone());
                    (
                        root,
                        pool.submit(path.clone(), move || {
                            open_root(&path, leaf.as_deref(), &allowed)
                        }),
                    )
                })
                .collect();
            if jobs.is_empty() {
                break;
            }
            for (root, job) in jobs {
                let result = match job {
                    Ok(job) => job.wait().await,
                    Err(error) => Err(error),
                };
                observed.insert(
                    root,
                    result
                        .map_err(RootCheckFailure::Pool)
                        .and_then(|probe| probe),
                );
            }
        }
        // A registry record may name a root without its file id. Fill that gap
        // from the identity of the root handle we actually opened, so a later
        // recheck can tell when the folder at that path was swapped, even when
        // the registry's own token is unchanged.
        for project in &mut self.projects {
            for root in project
                .project_roots
                .iter_mut()
                .chain(&mut project.workspace_root)
            {
                if let (None, Some(Ok(opened))) = (&root.file_id, observed.get(&root.path)) {
                    root.file_id = Some(opened.id.clone());
                }
            }
        }
        self.opened = Arc::new(observed);
    }

    fn opened_root(
        &self,
        project: &str,
        root: &RegistryRoot,
        rechecking: bool,
    ) -> Result<&OpenedRoot> {
        let opened = self
            .opened
            .get(&root.path)
            .ok_or_else(|| layer_unavailable("registry root was not captured"))?
            .as_ref()
            .map_err(|error| error.error(project, &root.path, rechecking))?;
        if root.file_id.as_ref() != Some(&opened.id) {
            return Err(RootCheckFailure::IdentityMismatch.error(project, &root.path, rechecking));
        }
        Ok(opened)
    }

    fn project(&self, id: &str) -> Result<&RegistryProject> {
        self.projects
            .iter()
            .find(|project| project.project_id == id)
            .ok_or_else(|| {
                GateError::new(
                    "unknown_project",
                    format!("{id}: project is not registered"),
                )
            })
    }

    /// Borrow a retained root and its layer observation for a registered project.
    pub fn layer_location(&self, id: &str, root: &Path) -> Result<(&File, &LayerLocation)> {
        let project = self.project(id)?;
        if let Some(error) = self.metadata_errors.get(id) {
            return Err(error.clone());
        }
        let mut opened = None;
        for record in project.roots().filter(|record| record.path == root) {
            opened = Some(self.opened_root(id, record, false)?);
        }
        let opened = opened.ok_or_else(|| {
            GateError::new(
                "layer_root_invalid",
                "project does not authorize this layer root",
            )
        })?;
        Ok((&opened.handle, location(opened, id, root)?))
    }

    /// Resolve present tuple inputs or return this project's refusal alone.
    pub fn project_layers(&self, id: &str) -> Result<ProjectLayers<'_>> {
        let project = self.project(id)?;
        if let Some(error) = self.metadata_errors.get(id) {
            return Err(error.clone());
        }
        let input = |root: &RegistryRoot| -> Result<_> {
            let opened = self.opened_root(id, root, false)?;
            Ok(location(opened, id, &root.path)?.file.as_ref())
        };
        let mut project_layer = None;
        for root in &project.project_roots {
            if let Some(file) = input(root)? {
                if project_layer.is_some() {
                    return Err(ambiguous(id));
                }
                project_layer = Some((root.path.as_path(), file));
            }
        }
        let workspace = if let Some(root) = &project.workspace_root {
            let file = input(root)?;
            file.filter(|_| !project_layer.is_some_and(|(path, _)| path == root.path))
                .map(|file| (root.path.as_path(), file))
        } else {
            None
        };
        Ok(ProjectLayers {
            workspace,
            project: project_layer,
        })
    }

    /// Include authorized locations even if the file is absent, unreadable,
    /// deleted or not selected as the tuple's current project layer.
    pub fn readers(&self, path: &Path) -> BTreeSet<String> {
        self.projects
            .iter()
            .filter(|project| {
                project
                    .roots()
                    .any(|root| layer_path(&root.path, &self.module) == path)
            })
            .map(|project| project.project_id.clone())
            .collect()
    }

    pub fn readers_before_or_proposed(&self, proposed: &Self, path: &Path) -> BTreeSet<String> {
        self.readers(path)
            .into_iter()
            .chain(proposed.readers(path))
            .collect()
    }

    pub fn digest(&self) -> Result<Digest> {
        subc_config::digest(
            subc_config::PURPOSE_SNAPSHOT,
            &serde_json::json!({"kind":"project-registry-v1","registry":self}),
        )
        .map_err(GateError::invalid)
    }

    pub(super) fn digest_for_project(&self, id: &str) -> Result<Digest> {
        subc_config::digest(
            subc_config::PURPOSE_SNAPSHOT,
            &serde_json::json!(["project-registry-entry-v1", self.module, self.project(id)?]),
        )
        .map_err(GateError::invalid)
    }

    /// Compare freshly derived affected entries and touched layer identities,
    /// not the enumeration generation or bytes of other layer files.
    pub fn recheck(&self, current: &Self, scope: &RegistryScope) -> Result<()> {
        fn affected<'a>(
            revision: &'a RegistryRevision,
            scope: &RegistryScope,
        ) -> BTreeMap<&'a str, &'a RegistryProject> {
            revision
                .projects
                .iter()
                .filter(|project| {
                    scope.includes(
                        &project.project_id,
                        project.workspace_id.as_ref(),
                        project.roots().map(|root| &root.path),
                        &revision.module,
                    )
                })
                .map(|project| (project.project_id.as_str(), project))
                .collect()
        }
        let before = affected(self, scope);
        let after = affected(current, scope);
        if self.module != current.module || before != after {
            return Err(changed());
        }
        for (id, project) in after {
            if let Some(error) = current.metadata_errors.get(id) {
                return Err(error.clone());
            }
            for root in project.roots() {
                let opened = current.opened_root(id, root, true)?;
                if !scope.paths.contains(&layer_path(&root.path, &self.module)) {
                    continue;
                }
                let previous = self.opened_root(id, root, true)?;
                let now = location(opened, id, &root.path).map_err(|error| {
                    if error.code.starts_with("layer_") {
                        changed()
                    } else {
                        error
                    }
                })?;
                if !location(previous, id, &root.path)?.same_identity(now) {
                    return Err(changed());
                }
            }
        }
        Ok(())
    }
}

fn location<'a>(opened: &'a OpenedRoot, project: &str, root: &Path) -> Result<&'a LayerLocation> {
    opened
        .layer
        .as_ref()
        .ok_or_else(|| layer_unavailable("registry layer was not captured"))?
        .as_ref()
        .map_err(|error| {
            GateError::new(
                &error.code,
                format!("{project}: root {}: {}", root.display(), error.detail),
            )
        })
}

fn parse_id(text: Option<&str>) -> Result<Option<RegistryFileId>> {
    text.map(RegistryFileId::parse).transpose()
}

fn validate_module(module: &str) -> Result<()> {
    if module.is_empty()
        || module.contains(['/', '\\', ':', '\0'])
        || module == "."
        || module == ".."
    {
        return Err(unavailable("module identity cannot form a layer path"));
    }
    Ok(())
}

pub(super) fn layer_path(root: &Path, module: &str) -> PathBuf {
    root.join(".cortexkit").join(format!("{module}.jsonc"))
}

/// Entorhinal's enumerate contract makes every listed root canonical; only one
/// eligible root may contain a given module's project layer.
/// Presence must come from opens beneath retained root handles, never from a
/// caller-proposed root or an unanchored pathname probe.
pub fn project_layer_root<'a>(
    project: &'a Project,
    module: &str,
    present: &BTreeSet<PathBuf>,
) -> Result<Option<&'a RootRecord>> {
    validate_module(module)?;
    let candidates: Vec<_> = project
        .root_records
        .iter()
        .flatten()
        .filter(|root| {
            eligible(root) && present.contains(&layer_path(Path::new(&root.root), module))
        })
        .collect();
    if candidates.len() > 1 {
        return Err(GateError::new(
            "layer_root_ambiguous",
            format!(
                "{}: multiple project layer roots: {}",
                project.project_id,
                candidates
                    .iter()
                    .map(|root| root.root.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    Ok(candidates.into_iter().next())
}

fn ambiguous(project: &str) -> GateError {
    GateError::new(
        "layer_root_ambiguous",
        format!("{project}: more than one project root contains the layer"),
    )
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
#[doc(hidden)]
pub type RegistryBatch = (Option<EnumerateReply>, BTreeMap<String, RegistryRevision>);
#[doc(hidden)]
pub type RegistryBatchFuture<'a> = Pin<Box<dyn Future<Output = Result<RegistryBatch>> + Send + 'a>>;

pub trait ProjectRegistry: Send + Sync {
    fn snapshot_scoped<'a>(
        &'a self,
        module: &'a str,
        scope: &'a RegistryScope,
    ) -> RegistryFuture<'a>;

    /// Capture several modules against one complete enumeration when the
    /// registry supplies it. Root I/O is independent of the runtime owner.
    #[doc(hidden)]
    fn capture_batch<'a>(
        &'a self,
        scopes: &'a BTreeMap<String, RegistryScope>,
    ) -> RegistryBatchFuture<'a> {
        Box::pin(async move {
            let mut revisions = BTreeMap::new();
            for (module, scope) in scopes {
                revisions.insert(module.clone(), self.snapshot_scoped(module, scope).await?);
            }
            Ok((None, revisions))
        })
    }

    fn snapshot<'a>(
        &'a self,
        module: &'a str,
        affected: &'a BTreeSet<String>,
    ) -> RegistryFuture<'a> {
        Box::pin(async move {
            let scope = RegistryScope {
                projects: affected.clone(),
                ..RegistryScope::default()
            };
            self.snapshot_scoped(module, &scope).await
        })
    }

    #[doc(hidden)]
    fn root_io_health(&self) -> Option<subc_control::config::ConfigRootIo> {
        None
    }

    fn recheck<'a>(
        &'a self,
        saved: &'a RegistryRevision,
        scope: &'a RegistryScope,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut scope = scope.clone();
            scope.capture = CaptureLayers::Targets;
            let current = self.snapshot_scoped(&saved.module, &scope).await?;
            saved.recheck(&current, &scope)
        })
    }
}

#[cfg(test)]
pub(crate) struct OfflineProjectRegistry;
#[cfg(test)]
impl ProjectRegistry for OfflineProjectRegistry {
    fn snapshot_scoped<'a>(&'a self, _: &'a str, _: &'a RegistryScope) -> RegistryFuture<'a> {
        Box::pin(async { Err(unavailable("entorhinal is not registered")) })
    }
}

#[cfg(test)]
mod tests;
