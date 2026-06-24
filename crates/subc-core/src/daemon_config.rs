use std::{
    collections::BTreeMap,
    env,
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use subc_jsonc::jsonc_to_json;

const DAEMON_CONFIG_RELATIVE_PATH: &str = "cortexkit/subc.jsonc";
const SUPPORTED_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    pub path: PathBuf,
    pub port: Option<u16>,
    pub modules: Vec<ConfiguredModule>,
    /// Central storage policy: the single backend choice all managed modules use.
    /// `None` when the config has no `storage` section (no managed storage).
    pub storage: Option<StorageConfig>,
}

/// Central storage configuration: one backend for every managed module. subc
/// resolves this into a per-module storage descriptor and delivers it in the
/// module's HELLO_ACK; the module opens it via the shared store library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageConfig {
    /// Each module gets its own sqlite file under `data_home`.
    Sqlite { data_home: PathBuf },
}

impl StorageConfig {
    /// Resolve this central policy into a module's storage descriptor: the opaque
    /// JSON delivered in `HELLO_ACK.storage`. The shape matches
    /// `cortexkit_store_types::StorageDescriptor` (subc constructs it by hand to
    /// avoid a database-library dependency in the thin daemon). The module
    /// deserializes it into that type and hands it to `cortexkit-store`.
    pub fn descriptor_for(&self, module_id: &str) -> serde_json::Value {
        match self {
            // Path convention mirrors cortexkit_store_types::sqlite_store_path:
            // <data_home>/cortexkit/<module_id>/store.db. One database per module;
            // a project-scoped module partitions its own rows internally.
            //
            // Build the path with forward slashes (NOT PathBuf::join, which inserts
            // backslashes on Windows) so the delivered wire descriptor is identical
            // cross-platform and byte-matches the store-types helper. Forward-slash
            // paths are accepted by sqlite on every platform.
            StorageConfig::Sqlite { data_home } => {
                let data_home = data_home.to_string_lossy();
                let path = format!(
                    "{}/cortexkit/{module_id}/store.db",
                    data_home.trim_end_matches('/')
                );
                serde_json::json!({
                    "module_id": module_id,
                    "storage_namespace": "default",
                    "isolation": { "kind": "module" },
                    "backend": { "backend": "sqlite", "path": path },
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredModule {
    pub module_id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub enabled: bool,
    /// When true, only the daemon-spawned process for this `module_id` may register
    /// it: subc injects a one-time launch nonce on spawn and rejects any HELLO for
    /// this id whose nonce does not match. Protects security-boundary modules (e.g.
    /// the credential vault) from being impersonated by another key-holder while the
    /// real process is down or restarting. Defaults to false.
    pub reserved: bool,
}

#[derive(Debug)]
pub enum DaemonConfigError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    InvalidJsonc {
        path: PathBuf,
        message: String,
    },
    InvalidJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    UnsupportedVersion {
        path: PathBuf,
        version: u32,
    },
}

#[derive(Debug, Deserialize)]
struct RawDaemonConfig {
    version: u32,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    modules: BTreeMap<String, RawModuleConfig>,
    #[serde(default)]
    storage: Option<RawStorageConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
enum RawStorageConfig {
    Sqlite {
        /// Where per-module sqlite files live. Defaults to the platform data home
        /// (`$XDG_DATA_HOME`, else `~/.local/share`) when omitted.
        #[serde(default)]
        data_home: Option<PathBuf>,
    },
}

#[derive(Debug, Deserialize)]
struct RawModuleConfig {
    program: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default)]
    reserved: bool,
}

pub fn default_config_path() -> PathBuf {
    if let Some(config_home) = non_empty_os_var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home).join(DAEMON_CONFIG_RELATIVE_PATH);
    }

    #[cfg(windows)]
    {
        if let Some(app_data) = non_empty_os_var("APPDATA") {
            return PathBuf::from(app_data).join("cortexkit").join("subc.jsonc");
        }
        if let Some(user_profile) = non_empty_os_var("USERPROFILE") {
            return PathBuf::from(user_profile)
                .join("AppData")
                .join("Roaming")
                .join("cortexkit")
                .join("subc.jsonc");
        }
    }

    if let Some(home) = non_empty_os_var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join(DAEMON_CONFIG_RELATIVE_PATH);
    }

    PathBuf::from(".config").join(DAEMON_CONFIG_RELATIVE_PATH)
}

pub fn load(path: impl AsRef<Path>) -> Result<Option<DaemonConfig>, DaemonConfigError> {
    let path = path.as_ref();
    let doc = match fs::read_to_string(path) {
        Ok(doc) => doc,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonConfigError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    parse_doc(&doc, path).map(Some)
}

fn parse_doc(doc: &str, path: &Path) -> Result<DaemonConfig, DaemonConfigError> {
    let json = jsonc_to_json(doc).map_err(|message| DaemonConfigError::InvalidJsonc {
        path: path.to_path_buf(),
        message,
    })?;
    let raw: RawDaemonConfig =
        serde_json::from_str(&json).map_err(|source| DaemonConfigError::InvalidJson {
            path: path.to_path_buf(),
            source,
        })?;

    if raw.version != SUPPORTED_CONFIG_VERSION {
        return Err(DaemonConfigError::UnsupportedVersion {
            path: path.to_path_buf(),
            version: raw.version,
        });
    }

    let modules = raw
        .modules
        .into_iter()
        .map(|(module_id, module)| ConfiguredModule {
            module_id,
            program: module.program,
            args: module.args,
            env: module.env.into_iter().collect(),
            enabled: module.enabled,
            reserved: module.reserved,
        })
        .collect();

    let storage = raw.storage.map(|s| match s {
        RawStorageConfig::Sqlite { data_home } => StorageConfig::Sqlite {
            data_home: data_home.unwrap_or_else(default_data_home),
        },
    });

    Ok(DaemonConfig {
        path: path.to_path_buf(),
        port: raw.port,
        modules,
        storage,
    })
}

fn default_enabled() -> bool {
    true
}

/// Platform data home for per-module storage: `$XDG_DATA_HOME`, else
/// `~/.local/share` (or the Windows roaming app data), else a relative fallback.
fn default_data_home() -> PathBuf {
    if let Some(data_home) = non_empty_os_var("XDG_DATA_HOME") {
        return PathBuf::from(data_home);
    }

    #[cfg(windows)]
    {
        if let Some(app_data) = non_empty_os_var("APPDATA") {
            return PathBuf::from(app_data);
        }
        if let Some(user_profile) = non_empty_os_var("USERPROFILE") {
            return PathBuf::from(user_profile).join("AppData").join("Roaming");
        }
    }

    if let Some(home) = non_empty_os_var("HOME") {
        return PathBuf::from(home).join(".local").join("share");
    }

    PathBuf::from(".local").join("share")
}

fn non_empty_os_var(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

impl fmt::Display for DaemonConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "failed to read daemon config {}: {source}", path.display())
            }
            Self::InvalidJsonc { path, message } => {
                write!(f, "invalid JSONC in daemon config {}: {message}", path.display())
            }
            Self::InvalidJson { path, source } => {
                write!(f, "invalid daemon config {}: {source}", path.display())
            }
            Self::UnsupportedVersion { path, version } => write!(
                f,
                "invalid daemon config {}: version {version} is unsupported (expected {SUPPORTED_CONFIG_VERSION})",
                path.display()
            ),
        }
    }
}

impl Error for DaemonConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::InvalidJson { source, .. } => Some(source),
            Self::InvalidJsonc { .. } | Self::UnsupportedVersion { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_storage_section_yields_none() {
        let config = parse_doc(
            r#"{ "version": 1, "modules": {} }"#,
            Path::new("/tmp/subc.jsonc"),
        )
        .expect("parse");
        assert_eq!(config.storage, None);
    }

    #[test]
    fn sqlite_storage_parses_with_explicit_data_home() {
        let config = parse_doc(
            r#"{ "version": 1, "storage": { "backend": "sqlite", "data_home": "/data" } }"#,
            Path::new("/tmp/subc.jsonc"),
        )
        .expect("parse");
        assert_eq!(
            config.storage,
            Some(StorageConfig::Sqlite {
                data_home: PathBuf::from("/data")
            })
        );
    }

    #[test]
    fn sqlite_storage_defaults_data_home_when_omitted() {
        // With no data_home, it falls back to the platform data home (here forced
        // via XDG_DATA_HOME so the test is deterministic).
        std::env::set_var("XDG_DATA_HOME", "/forced/data/home");
        let config = parse_doc(
            r#"{ "version": 1, "storage": { "backend": "sqlite" } }"#,
            Path::new("/tmp/subc.jsonc"),
        )
        .expect("parse");
        std::env::remove_var("XDG_DATA_HOME");
        assert_eq!(
            config.storage,
            Some(StorageConfig::Sqlite {
                data_home: PathBuf::from("/forced/data/home")
            })
        );
    }

    #[test]
    fn descriptor_for_matches_store_types_shape() {
        // The opaque descriptor subc delivers must match the
        // cortexkit_store_types::StorageDescriptor JSON shape exactly (path
        // convention <data_home>/cortexkit/<module>/store.db, one db per module).
        let cfg = StorageConfig::Sqlite {
            data_home: PathBuf::from("/data"),
        };
        let descriptor = cfg.descriptor_for("alfonso-routing");
        assert_eq!(
            descriptor,
            serde_json::json!({
                "module_id": "alfonso-routing",
                "storage_namespace": "default",
                "isolation": { "kind": "module" },
                "backend": {
                    "backend": "sqlite",
                    "path": "/data/cortexkit/alfonso-routing/store.db"
                }
            })
        );
    }

    #[test]
    fn parse_jsonc_defaults_and_ignores_unknown_fields() {
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"
            {
              // forward-compatible root field
              "version": 1,
              "unknown": { "ignored": true },
              "modules": {
                "aft": {
                  "program": "aft",
                  "args": ["module",],
                  "env": { "A": "B", },
                  "future": 42,
                },
                "disabled": { "program": "disabled", "enabled": false }
              },
            }
            "#,
            path,
        )
        .unwrap();

        assert_eq!(config.port, None);
        assert_eq!(config.modules.len(), 2);
        assert_eq!(config.modules[0].module_id, "aft");
        assert_eq!(config.modules[0].program, PathBuf::from("aft"));
        assert_eq!(config.modules[0].args, ["module"]);
        assert_eq!(config.modules[0].env, [("A".to_string(), "B".to_string())]);
        assert!(config.modules[0].enabled);
        assert!(!config.modules[1].enabled);
    }

    #[test]
    fn reject_unsupported_version() {
        let err = parse_doc(
            r#"{ "version": 2, "modules": {} }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DaemonConfigError::UnsupportedVersion { version: 2, .. }
        ));
    }

    #[test]
    fn reject_unterminated_block_comment() {
        let err = parse_doc(r#"{ "version": 1, /*"#, Path::new("subc.jsonc")).unwrap_err();
        assert!(matches!(err, DaemonConfigError::InvalidJsonc { .. }));
    }
}
