use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

/// Stable canonical identity for a project root.
///
/// A `ProjectRootId` is represented by the canonical filesystem path of an
/// existing project root. Construction uses [`std::fs::canonicalize`], so the
/// stored path is absolute, has `.`/`..`/trailing separators collapsed, and has
/// symlinks resolved.
///
/// Git worktrees are first-class roots: subc does not ask Git for a repository
/// common-dir and does not collapse linked worktrees back to their main
/// checkout. Because a linked worktree has its own checkout directory, the
/// canonical worktree path is a distinct id from the canonical main-checkout
/// path while alternate spellings of either path still converge.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectRootId(PathBuf);

impl ProjectRootId {
    /// Resolve an existing filesystem path into a canonical project-root id.
    ///
    /// Non-existent paths are rejected with [`IdentityError::NonExistentPath`]
    /// instead of being logically normalized. That policy avoids silently
    /// aliasing roots whose future meaning could change when missing path
    /// components or symlinks are later created.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, IdentityError> {
        let requested_path = path.as_ref().to_path_buf();
        match fs::canonicalize(path.as_ref()) {
            Ok(canonical_path) => Ok(Self(canonical_path)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                Err(IdentityError::NonExistentPath {
                    path: requested_path,
                })
            }
            Err(source) => Err(IdentityError::CanonicalizePath {
                path: requested_path,
                source,
            }),
        }
    }

    /// Borrow the canonical path backing this identity.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume the identity and return its canonical path representation.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for ProjectRootId {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl fmt::Display for ProjectRootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl From<ProjectRootId> for PathBuf {
    fn from(value: ProjectRootId) -> Self {
        value.into_path_buf()
    }
}

impl TryFrom<&Path> for ProjectRootId {
    type Error = IdentityError;

    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        Self::from_path(value)
    }
}

impl TryFrom<PathBuf> for ProjectRootId {
    type Error = IdentityError;

    fn try_from(value: PathBuf) -> Result<Self, Self::Error> {
        Self::from_path(value)
    }
}

impl TryFrom<&str> for ProjectRootId {
    type Error = IdentityError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_path(Path::new(value))
    }
}

impl TryFrom<String> for ProjectRootId {
    type Error = IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_path(PathBuf::from(value))
    }
}

/// Opaque session identifier supplied by the harness.
///
/// subc treats this as an uninterpreted string and only carries it through to
/// modules that need per-session undo, backup, bash-task, or checkpoint state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Wrap a harness-supplied opaque session id without interpreting it.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the opaque session id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the session id and return the opaque string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.into_string()
    }
}

/// Identity carried by routed requests.
///
/// The session id scopes per-session state while the project root id scopes
/// project-shared state and is suitable for lease/scheduler map keys.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestIdentity {
    pub session_id: SessionId,
    pub project_root: ProjectRootId,
}

impl RequestIdentity {
    pub fn new(session_id: impl Into<SessionId>, project_root: ProjectRootId) -> Self {
        Self {
            session_id: session_id.into(),
            project_root,
        }
    }

    pub fn from_path(
        session_id: impl Into<SessionId>,
        project_root: impl AsRef<Path>,
    ) -> Result<Self, IdentityError> {
        Ok(Self::new(
            session_id,
            ProjectRootId::from_path(project_root)?,
        ))
    }
}

/// Typed identity-resolution failures.
#[derive(Debug)]
pub enum IdentityError {
    /// The requested project root does not exist, or a path component cannot be
    /// resolved through an existing symlink chain.
    NonExistentPath { path: PathBuf },
    /// The OS rejected canonicalization for a reason other than non-existence.
    CanonicalizePath { path: PathBuf, source: io::Error },
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonExistentPath { path } => {
                write!(f, "project root does not exist: {}", path.display())
            }
            Self::CanonicalizePath { path, source } => {
                write!(
                    f,
                    "failed to canonicalize project root {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for IdentityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NonExistentPath { .. } => None,
            Self::CanonicalizePath { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let unique = format!(
                "subc-core-identity-{label}-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system time should not be before the Unix epoch")
                    .as_nanos(),
                NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            fs::create_dir(&path).expect("create temporary identity test directory");
            Self { path }
        }

        fn child(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn path_spellings_to_same_root_have_equal_project_root_ids() {
        let temp = TestDir::new("spellings");
        let root = temp.child("project");
        let nested = root.join("nested");
        fs::create_dir(&root).expect("create project root");
        fs::create_dir(&nested).expect("create nested directory");

        let trailing = PathBuf::from(format!("{}{}", root.display(), std::path::MAIN_SEPARATOR));
        let direct = ProjectRootId::from_path(&root).expect("canonicalize direct root");
        let with_trailing = ProjectRootId::from_path(trailing).expect("canonicalize trailing root");
        let with_dot = ProjectRootId::from_path(root.join(".")).expect("canonicalize dot root");
        let round_trip =
            ProjectRootId::from_path(nested.join("..")).expect("canonicalize round-trip root");

        assert_eq!(direct, with_trailing);
        assert_eq!(direct, with_dot);
        assert_eq!(direct, round_trip);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_project_root_has_same_id_as_target() {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new("symlink");
        let target = temp.child("target");
        let link = temp.child("link");
        fs::create_dir(&target).expect("create symlink target");
        symlink(&target, &link).expect("create symlink to project root");

        let target_id = ProjectRootId::from_path(&target).expect("canonicalize target");
        let link_id = ProjectRootId::from_path(&link).expect("canonicalize symlink");

        assert_eq!(target_id, link_id);
    }

    #[test]
    fn git_worktree_checkout_path_is_distinct_from_main_checkout_path() {
        let temp = TestDir::new("worktree");
        let main_checkout = temp.child("main-checkout");
        let linked_worktree = temp.child("linked-worktree");
        let main_gitdir = main_checkout.join(".git");
        let worktree_gitdir = main_gitdir.join("worktrees").join("linked-worktree");

        fs::create_dir(&main_checkout).expect("create main checkout");
        fs::create_dir(&linked_worktree).expect("create linked worktree checkout");
        fs::create_dir_all(&worktree_gitdir).expect("create simulated worktree gitdir");
        fs::write(
            linked_worktree.join(".git"),
            format!("gitdir: {}\n", worktree_gitdir.display()),
        )
        .expect("write simulated linked-worktree .git file");

        let main_id = ProjectRootId::from_path(&main_checkout).expect("canonicalize main checkout");
        let worktree_id =
            ProjectRootId::from_path(&linked_worktree).expect("canonicalize linked worktree");

        assert_ne!(main_id, worktree_id);
    }

    #[test]
    fn non_existent_project_root_returns_typed_error() {
        let temp = TestDir::new("missing");
        let missing_root = temp.child("missing-project");

        match ProjectRootId::from_path(&missing_root) {
            Err(IdentityError::NonExistentPath { path }) => assert_eq!(path, missing_root),
            Err(other) => panic!("expected NonExistentPath error, got {other}"),
            Ok(id) => panic!("expected missing project root to fail, got {id}"),
        }
    }

    #[test]
    fn request_identity_is_hashable_as_hash_map_key() {
        let temp = TestDir::new("hashmap");
        let root = temp.child("project");
        fs::create_dir(&root).expect("create project root");

        let identity = RequestIdentity::from_path("ses_a", &root).expect("build request identity");
        let same_identity = RequestIdentity::from_path(String::from("ses_a"), root.join("."))
            .expect("build equivalent request identity");
        let other_session = RequestIdentity::from_path("ses_b", &root)
            .expect("build different-session request identity");

        let mut entries = HashMap::new();
        entries.insert(identity.clone(), "session A project state");

        assert_eq!(
            entries.get(&same_identity),
            Some(&"session A project state")
        );
        assert_eq!(entries.get(&other_session), None);
    }
}
