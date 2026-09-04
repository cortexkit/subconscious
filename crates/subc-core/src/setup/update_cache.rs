use std::{collections::BTreeMap, env, fs, io::ErrorKind, path::PathBuf, process, time::Duration};

use serde::{Deserialize, Serialize};

pub const UPDATE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Bumped when the cache document began retaining the host asset digest. Older
/// files cannot decide currency and are treated as absent so one check rebuilds
/// them from the signed release index.
pub const UPDATE_CACHE_FORMAT_VERSION: u32 = 3;

/// Release evidence retained after a successful user-initiated update check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedRelease {
    /// Display-only release text; sibling binaries need not share it.
    pub version: String,
    /// Digest of this binary's asset for the invoking host, or `None` when the
    /// signed index has no matching asset for that target.
    pub sha256: Option<String>,
}

/// The on-disk, user-owned update metadata cache. Daemon code does not import
/// this module, so release metadata can only be read or refreshed by `ck`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateMetadata {
    #[serde(default)]
    pub format_version: u32,
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
    /// `None` when no per-user cache directory resolves on this host: the
    /// cache then reads as absent and writes nothing, so `ck` re-checks
    /// within its budget instead of persisting somewhere surprising.
    path: Option<PathBuf>,
}

impl UpdateCache {
    pub fn from_environment() -> Self {
        Self {
            path: default_cache_path(),
        }
    }

    /// Only tests pin a cache to an explicit path; production resolves it
    /// from the environment.
    #[cfg(test)]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    #[cfg(test)]
    pub fn path(&self) -> &PathBuf {
        self.path.as_ref().expect("test caches always have a path")
    }

    pub fn load(&self) -> CacheRead {
        let Some(path) = &self.path else {
            return CacheRead::Absent;
        };
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return CacheRead::Absent,
            Err(error) => return CacheRead::Unreadable(error.to_string()),
        };
        match serde_json::from_slice::<UpdateMetadata>(&bytes) {
            Ok(metadata) if metadata.format_version == UPDATE_CACHE_FORMAT_VERSION => {
                CacheRead::Present(metadata)
            }
            Ok(_) => CacheRead::Absent,
            Err(_) => CacheRead::Malformed,
        }
    }

    /// Replaces the complete metadata document after its release-source request
    /// succeeds. A temporary sibling prevents a truncated cache from replacing a
    /// prior good observation if this process is interrupted while writing.
    pub fn write(&self, metadata: &UpdateMetadata) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let Some(parent) = path.parent() else {
            return Err(format!("cache path {} has no parent", path.display()));
        };
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec(metadata).map_err(|error| error.to_string())?;
        let temporary = path.with_extension(format!("tmp-{}", process::id()));
        fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error.to_string());
        }
        Ok(())
    }
}

fn default_cache_path() -> Option<PathBuf> {
    if let Some(path) = non_empty_os_var("CK_UPDATE_CACHE_PATH") {
        return Some(PathBuf::from(path));
    }
    cache_directory().map(|directory| directory.join("update-metadata.json"))
}

/// The per-user directory every `ck` cache lives under. There is deliberately
/// no relative fallback: a cache written relative to the working directory
/// lands in whatever the user or a test runner happened to be standing in
/// (a Windows runner with no `HOME` once wrote it into the source tree).
/// When nothing resolves, `ck` runs without a cache.
pub fn cache_directory() -> Option<PathBuf> {
    if let Some(cache_home) = non_empty_os_var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(cache_home).join("cortexkit"));
    }
    #[cfg(windows)]
    if let Some(local_app_data) = non_empty_os_var("LOCALAPPDATA") {
        return Some(
            PathBuf::from(local_app_data)
                .join("cortexkit")
                .join("cache"),
        );
    }
    non_empty_os_var("HOME").map(|home| PathBuf::from(home).join(".cache").join("cortexkit"))
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
            format_version: UPDATE_CACHE_FORMAT_VERSION,
            checked_at_unix_secs,
            targets: BTreeMap::from([(
                "ck".to_string(),
                CachedRelease {
                    version: "0.13.0".to_string(),
                    sha256: Some("ab".repeat(32)),
                },
            )]),
        }
    }

    #[test]
    fn old_cache_format_is_treated_as_absent() {
        let _dir = TestTempDir::new("old-format");
        let path = _dir.path().join("update-metadata.json");
        let cache = UpdateCache::new(&path);
        fs::write(&path, r#"{"checked_at_unix_secs":1,"targets":{}}"#).unwrap();
        assert_eq!(cache.load(), CacheRead::Absent);
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
