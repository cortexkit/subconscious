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

use crate::{HealthAction, HealthConfig};

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
    /// Namespace prefixes owned by this reserved, supervised module. A HELLO for a
    /// module id under one of these prefixes must echo this owner module's current
    /// spawn nonce.
    pub reserved_prefixes: Vec<String>,
    pub health: HealthConfig,
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
    InvalidValue {
        path: PathBuf,
        message: String,
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
    #[serde(default)]
    reserved_prefixes: Vec<String>,
    #[serde(default)]
    health: Option<RawHealthConfig>,
}

#[derive(Debug, Deserialize)]
struct RawHealthConfig {
    #[serde(default)]
    cadence_ms: Option<u64>,
    #[serde(default)]
    deadline_ms: Option<u64>,
    #[serde(default)]
    failure_threshold: Option<u32>,
    #[serde(default)]
    on_degraded: Option<RawHealthAction>,
    #[serde(default)]
    on_failing: Option<RawHealthAction>,
    #[serde(default)]
    critical: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawHealthAction {
    Report,
    Restart,
    Alert,
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
        .map(|(module_id, module)| {
            let health = module
                .health
                .map(|health| parse_health_config(health, path, &module_id))
                .transpose()?
                .unwrap_or_default();
            Ok(ConfiguredModule {
                module_id,
                program: module.program,
                args: module.args,
                env: module.env.into_iter().collect(),
                enabled: module.enabled,
                reserved: module.reserved,
                reserved_prefixes: module.reserved_prefixes,
                health,
            })
        })
        .collect::<Result<Vec<_>, DaemonConfigError>>()?;

    validate_reserved_prefixes(&modules, path)?;

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

fn validate_reserved_prefixes(
    modules: &[ConfiguredModule],
    path: &Path,
) -> Result<(), DaemonConfigError> {
    for module in modules {
        if module.reserved_prefixes.is_empty() {
            continue;
        }
        if !module.reserved {
            return Err(DaemonConfigError::InvalidValue {
                path: path.to_path_buf(),
                message: format!(
                    "module '{}' reserved_prefixes require reserved=true so the owner is spawn-nonce protected",
                    module.module_id
                ),
            });
        }
        for prefix in &module.reserved_prefixes {
            if !prefix.ends_with(':') {
                return Err(DaemonConfigError::InvalidValue {
                    path: path.to_path_buf(),
                    message: format!(
                        "module '{}' reserved prefix '{}' must end with ':'",
                        module.module_id, prefix
                    ),
                });
            }
        }
    }

    for module in modules {
        for prefix in &module.reserved_prefixes {
            if let Some(colliding) = modules
                .iter()
                .find(|candidate| candidate.module_id.starts_with(prefix))
            {
                return Err(DaemonConfigError::InvalidValue {
                    path: path.to_path_buf(),
                    message: format!(
                        "reserved prefix '{}' owned by '{}' collides with configured module id '{}'",
                        prefix, module.module_id, colliding.module_id
                    ),
                });
            }
        }
    }

    for (left_index, left) in modules.iter().enumerate() {
        for right in modules.iter().skip(left_index + 1) {
            if left.module_id == right.module_id {
                continue;
            }
            for left_prefix in &left.reserved_prefixes {
                for right_prefix in &right.reserved_prefixes {
                    if left_prefix.starts_with(right_prefix)
                        || right_prefix.starts_with(left_prefix)
                    {
                        return Err(DaemonConfigError::InvalidValue {
                            path: path.to_path_buf(),
                            message: format!(
                                "reserved prefixes '{}' owned by '{}' and '{}' owned by '{}' overlap",
                                left_prefix, left.module_id, right_prefix, right.module_id
                            ),
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

fn parse_health_config(
    raw: RawHealthConfig,
    path: &Path,
    module_id: &str,
) -> Result<HealthConfig, DaemonConfigError> {
    let defaults = HealthConfig::default();
    let cadence = positive_millis(
        raw.cadence_ms,
        defaults.cadence,
        path,
        module_id,
        "cadence_ms",
    )?;
    let deadline = positive_millis(
        raw.deadline_ms,
        defaults.deadline,
        path,
        module_id,
        "deadline_ms",
    )?;
    let failure_threshold = match raw.failure_threshold {
        Some(0) => {
            return Err(DaemonConfigError::InvalidValue {
                path: path.to_path_buf(),
                message: format!("module '{module_id}' health.failure_threshold must be positive"),
            })
        }
        Some(value) => value,
        None => defaults.failure_threshold,
    };

    Ok(HealthConfig {
        cadence,
        deadline,
        failure_threshold,
        on_degraded: raw
            .on_degraded
            .map(health_action)
            .unwrap_or(defaults.on_degraded),
        on_failing: raw
            .on_failing
            .map(health_action)
            .unwrap_or(defaults.on_failing),
        critical: raw.critical,
    })
}

fn positive_millis(
    value: Option<u64>,
    default: std::time::Duration,
    path: &Path,
    module_id: &str,
    field: &str,
) -> Result<std::time::Duration, DaemonConfigError> {
    match value {
        Some(0) => Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: format!("module '{module_id}' health.{field} must be positive"),
        }),
        Some(value) => Ok(std::time::Duration::from_millis(value)),
        None => Ok(default),
    }
}

fn health_action(action: RawHealthAction) -> HealthAction {
    match action {
        RawHealthAction::Report => HealthAction::Report,
        RawHealthAction::Restart => HealthAction::Restart,
        RawHealthAction::Alert => HealthAction::Alert,
    }
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
            Self::InvalidValue { path, message } => {
                write!(f, "invalid daemon config {}: {message}", path.display())
            }
        }
    }
}

impl Error for DaemonConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::InvalidJson { source, .. } => Some(source),
            Self::InvalidJsonc { .. }
            | Self::UnsupportedVersion { .. }
            | Self::InvalidValue { .. } => None,
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
        assert!(config.modules[0].reserved_prefixes.is_empty());
        assert_eq!(config.modules[0].health, HealthConfig::default());
        assert!(!config.modules[1].enabled);
    }

    #[test]
    fn reserved_prefixes_parse_for_reserved_modules() {
        let config = parse_doc(
            r#"
            {
              "version": 1,
              "modules": {
                "federation": {
                  "program": "fed",
                  "reserved": true,
                  "reserved_prefixes": ["fed:"]
                }
              }
            }
            "#,
            Path::new("subc.jsonc"),
        )
        .unwrap();

        assert_eq!(config.modules[0].reserved_prefixes, ["fed:".to_string()]);
    }

    #[test]
    fn reserved_prefixes_reject_bad_boundaries_and_owners() {
        let missing_delimiter = parse_doc(
            r#"{
              "version": 1,
              "modules": {
                "federation": { "program": "fed", "reserved": true, "reserved_prefixes": ["fed"] }
              }
            }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(matches!(
            missing_delimiter,
            DaemonConfigError::InvalidValue { .. }
        ));

        let non_reserved_owner = parse_doc(
            r#"{
              "version": 1,
              "modules": {
                "federation": { "program": "fed", "reserved_prefixes": ["fed:"] }
              }
            }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(matches!(
            non_reserved_owner,
            DaemonConfigError::InvalidValue { .. }
        ));
    }

    #[test]
    fn reserved_prefixes_reject_cross_owner_overlap_and_exact_id_collisions() {
        let overlap = parse_doc(
            r#"{
              "version": 1,
              "modules": {
                "fed-owner": { "program": "fed", "reserved": true, "reserved_prefixes": ["fed:"] },
                "sub-owner": { "program": "fed-sub", "reserved": true, "reserved_prefixes": ["fed:sub:"] }
              }
            }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(matches!(overlap, DaemonConfigError::InvalidValue { .. }));

        let exact_collision = parse_doc(
            r#"{
              "version": 1,
              "modules": {
                "federation": { "program": "fed", "reserved": true, "reserved_prefixes": ["fed:"] },
                "fed:special": { "program": "special" }
              }
            }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(matches!(
            exact_collision,
            DaemonConfigError::InvalidValue { .. }
        ));
    }

    #[test]
    fn health_config_parses_and_ignores_unknown_fields() {
        let config = parse_doc(
            r#"
            {
              "version": 1,
              "modules": {
                "aft": {
                  "program": "aft",
                  "health": {
                    "cadence_ms": 100,
                    "deadline_ms": 20,
                    "failure_threshold": 2,
                    "on_degraded": "report",
                    "on_failing": "restart",
                    "critical": true,
                    "future": "ignored"
                  }
                }
              }
            }
            "#,
            Path::new("subc.jsonc"),
        )
        .unwrap();

        let health = config.modules[0].health;
        assert_eq!(health.cadence, std::time::Duration::from_millis(100));
        assert_eq!(health.deadline, std::time::Duration::from_millis(20));
        assert_eq!(health.failure_threshold, 2);
        assert_eq!(health.on_degraded, HealthAction::Report);
        assert_eq!(health.on_failing, HealthAction::Restart);
        assert!(health.critical);
    }

    #[test]
    fn health_config_rejects_bad_enum_and_non_positive_numbers() {
        let bad_enum = parse_doc(
            r#"{
              "version": 1,
              "modules": { "aft": { "program": "aft", "health": { "on_failing": "page" } } }
            }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(matches!(bad_enum, DaemonConfigError::InvalidJson { .. }));

        let zero = parse_doc(
            r#"{
              "version": 1,
              "modules": { "aft": { "program": "aft", "health": { "cadence_ms": 0 } } }
            }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(matches!(zero, DaemonConfigError::InvalidValue { .. }));
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
