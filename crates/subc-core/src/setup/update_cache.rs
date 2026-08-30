use std::{collections::BTreeMap, env, fs, io::ErrorKind, path::PathBuf, process, time::Duration};

use serde::{Deserialize, Serialize};

pub const UPDATE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Release evidence retained after a successful user-initiated update check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedRelease {
    pub version: String,
    pub assets: Vec<String>,
}

/// The on-disk, user-owned update metadata cache. Daemon code does not import
/// this module, so release metadata can only be read or refreshed by `ck`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateMetadata {
    pub checked_at_unix_secs: u64,
    pub targets: BTreeMap<String, CachedRelease>,
}

impl UpdateMetadata {
    pub fn age_at(&self, now_unix_secs: u64) -> Duration {
        Duration::from_secs(now_unix_secs.saturating_sub(self.checked_at_unix_secs))
    }

    pub fn is_fresh_at(&self, now_unix_secs: u64) -> bool {
        self.age_at(now_unix_secs) < UPDATE_CACHE_TTL
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheRead {
    Absent,
    Malformed,
    Unreadable(String),
    Present(UpdateMetadata),
}

/// A file-backed cache whose location remains entirely in the invoking user's
/// cache directory. `CK_UPDATE_CACHE_PATH` is an explicit test and operator
/// override; it never changes daemon behavior because the daemon has no caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateCache {
    path: PathBuf,
}

impl UpdateCache {
    pub fn from_environment() -> Self {
        Self::new(default_cache_path())
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[cfg(test)]
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn load(&self) -> CacheRead {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return CacheRead::Absent,
            Err(error) => return CacheRead::Unreadable(error.to_string()),
        };
        match serde_json::from_slice(&bytes) {
            Ok(metadata) => CacheRead::Present(metadata),
            Err(_) => CacheRead::Malformed,
        }
    }

    /// Replaces the complete metadata document after its release-source request
    /// succeeds. A temporary sibling prevents a truncated cache from replacing a
    /// prior good observation if this process is interrupted while writing.
    pub fn write(&self, metadata: &UpdateMetadata) -> Result<(), String> {
        let Some(parent) = self.path.parent() else {
            return Err(format!("cache path {} has no parent", self.path.display()));
        };
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec(metadata).map_err(|error| error.to_string())?;
        let temporary = self.path.with_extension(format!("tmp-{}", process::id()));
        fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
        if let Err(error) = fs::rename(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(error.to_string());
        }
        Ok(())
    }
}

fn default_cache_path() -> PathBuf {
    if let Some(path) = non_empty_os_var("CK_UPDATE_CACHE_PATH") {
        return PathBuf::from(path);
    }
    if let Some(cache_home) = non_empty_os_var("XDG_CACHE_HOME") {
        return PathBuf::from(cache_home)
            .join("cortexkit")
            .join("update-metadata.json");
    }
    if let Some(home) = non_empty_os_var("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("cortexkit")
            .join("update-metadata.json");
    }
    PathBuf::from(".cache")
        .join("cortexkit")
        .join("update-metadata.json")
}

fn non_empty_os_var(key: &str) -> Option<std::ffi::OsString> {
    env::var_os(key).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::*;
    use subc_core::test_support::TestTempDir;

    fn metadata(checked_at_unix_secs: u64) -> UpdateMetadata {
        UpdateMetadata {
            checked_at_unix_secs,
            targets: BTreeMap::from([(
                "ck".to_string(),
                CachedRelease {
                    version: "0.13.0".to_string(),
                    assets: vec!["ck-darwin-arm64.zip".to_string()],
                },
            )]),
        }
    }

    #[test]
    fn fresh_metadata_remains_within_the_twenty_four_hour_ttl() {
        let metadata = metadata(10_000);
        assert!(metadata.is_fresh_at(10_000 + UPDATE_CACHE_TTL.as_secs() - 1));
        assert!(!metadata.is_fresh_at(10_000 + UPDATE_CACHE_TTL.as_secs()));
    }

    #[test]
    fn absent_and_malformed_cache_files_are_distinct_from_valid_metadata() {
        let _dir = TestTempDir::new("read-states");
        let path = _dir.path().join("update-metadata.json");
        let cache = UpdateCache::new(&path);
        assert_eq!(cache.load(), CacheRead::Absent);

        fs::write(&path, b"not json").unwrap();
        assert_eq!(cache.load(), CacheRead::Malformed);

        let expected = metadata(1_000);
        cache.write(&expected).unwrap();
        assert_eq!(cache.load(), CacheRead::Present(expected));
    }
}
