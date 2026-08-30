use std::{fmt, path::Path};

pub use cortexkit_paths::{IdentityError, ProjectRootId};

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

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs};

    use super::*;
    use crate::test_support::TestTempDir as TestDir;

    #[test]
    fn request_identity_is_hashable_as_hash_map_key() {
        let temp = TestDir::new("hashmap");
        let root = temp.path().join("project");
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
