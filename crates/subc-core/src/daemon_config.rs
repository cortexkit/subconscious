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
use subc_protocol::manifest::is_valid_capability_identifier;

use crate::{HealthAction, HealthConfig, ModuleSpec};

const DAEMON_CONFIG_RELATIVE_PATH: &str = "cortexkit/subc.jsonc";
const SUPPORTED_CONFIG_VERSION: u32 = 1;

/// Top-level daemon config sections that rescan cannot apply. The live daemon
/// keeps the values it loaded at start for these; a later rescan reports them
/// as `restart_required`. Setup intersects this set with the sections core
/// configuration would write so dry-run can show a restart before the file
/// exists on disk. Rescan itself reads the same table so the names cannot drift.
pub const RESTART_REQUIRED_SECTIONS: &[&str] = &[
    "port",
    "storage",
    "admission_facts_carrier_module_id",
    "admission_facts_targets",
];

/// Refused at parse time by both layers (daemon-wide and per-module) — `0`
/// would turn every affected bind into an instant failure, which is not a
/// posture anyone deliberately configures. The asymmetry with
/// `drain_timeout_ms` (which accepts `0` as a legitimate "tear down now")
/// is intentional: drain `0` is an *action* an operator takes during a
/// wedge bounce; bind `0` is a typo wearing a config key. Operators who
/// want a module unreachable should use `enabled: false` instead.
///
/// The per-module variant prefixes the offending module id before this
/// message — see `parse_doc`.
const ROUTE_BIND_RELAY_ZERO_MESSAGE: &str = "route_bind_relay_timeout_ms must be greater than 0 (a zero budget fails every bind to the module; to make a module unreachable use enabled: false)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    pub path: PathBuf,
    pub port: Option<u16>,
    /// Daemon-wide default drain budget (ms) for module teardown: how long a
    /// drain waits for already-dispatched requests to finalize. `None` uses
    /// the built-in default (30s). Per-module `drain_timeout_ms` overrides.
    pub drain_timeout_ms: Option<u64>,
    /// Daemon-wide default route.bind relay budget (ms): how long the daemon
    /// waits for the target module to acknowledge a relayed `route.bind` before
    /// reporting `module_timeout`. `None` uses the built-in default (12s, set
    /// in `control::DEFAULT_ROUTE_BIND_RELAY_TIMEOUT`). Per-module
    /// `route_bind_relay_timeout_ms` overrides. `0` is refused at parse time
    /// (a zero budget fails every bind; use `enabled: false` to make a
    /// module unreachable) — this is deliberately asymmetric with
    /// `drain_timeout_ms`, where `0` is the sanctioned "tear down now".
    pub route_bind_relay_timeout_ms: Option<u64>,
    pub modules: Vec<ConfiguredModule>,
    /// Central storage policy: the single backend choice all managed modules use.
    /// `None` when the config has no `storage` section (no managed storage).
    pub storage: Option<StorageConfig>,
    /// Exact module id whose reserved process may carry admission facts.
    pub admission_facts_carrier_module_id: Option<String>,
    /// Exact target module ids that may receive facts from the configured carrier.
    pub admission_facts_targets: Option<Vec<String>>,
    /// Capability names reserved to one module id. The binding may name a module
    /// that is not configured yet so an operator can reserve an interface before
    /// installing its provider.
    pub reserved_capabilities: BTreeMap<String, String>,
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
    ///
    /// THE DESCRIPTOR IS ADVISORY, NOT BINDING, and the daemon has no way to
    /// tell whether a module consumed it. A module that opens its store BEFORE
    /// connecting -- building its own descriptor from an environment variable --
    /// never reads this at all, and nothing on the wire reports that.
    ///
    /// Two consequences worth knowing before reasoning from a store path:
    ///
    /// * A store at the path below does NOT prove the descriptor arrived or was
    ///   keyed correctly; a self-keying module can land on the same path by
    ///   agreeing with the convention rather than by consuming the descriptor.
    ///   Any test asserting "the store landed under MODULE_ID" proves the
    ///   daemon's half only for modules that derive the path from the id they
    ///   claimed.
    /// * Where a self-keying module disagrees, BOTH paths can exist. Observed on
    ///   the live box: astrocyte is handed a data dir already ending in
    ///   `cortexkit/astrocyte` and appends the same suffix again, so its real
    ///   store sits nested while an empty file remains at the path this function
    ///   names -- and a reader inspecting that directory would reasonably
    ///   conclude the module has an empty store.
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
    /// Effective drain budget (ms) for this module's teardown, already resolved
    /// against the daemon-wide default at parse time. `None` = built-in default.
    pub drain_timeout_ms: Option<u64>,
    /// Effective route.bind relay budget (ms) for this module, already resolved
    /// against the daemon-wide default at parse time. `None` = built-in default
    /// (12s). A `0` is refused at parse time at both layers — see
    /// `DaemonConfig::route_bind_relay_timeout_ms` and `ROUTE_BIND_RELAY_ZERO_MESSAGE`.
    pub route_bind_relay_timeout_ms: Option<u64>,
}

impl ConfiguredModule {
    pub fn module_spec(&self) -> ModuleSpec {
        ModuleSpec {
            module_id: self.module_id.clone(),
            program: self.program.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            reserved: self.reserved,
            reserved_prefixes: self.reserved_prefixes.clone(),
        }
    }
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
    drain_timeout_ms: Option<u64>,
    #[serde(default)]
    route_bind_relay_timeout_ms: Option<u64>,
    #[serde(default)]
    modules: BTreeMap<String, RawModuleConfig>,
    #[serde(default)]
    storage: Option<RawStorageConfig>,
    #[serde(default)]
    admission_facts_carrier_module_id: Option<String>,
    #[serde(default)]
    admission_facts_targets: Option<Vec<String>>,
    #[serde(default)]
    reserved_capabilities: BTreeMap<String, String>,
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
    #[serde(default)]
    drain_timeout_ms: Option<u64>,
    #[serde(default)]
    route_bind_relay_timeout_ms: Option<u64>,
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

    let default_drain_timeout_ms = raw.drain_timeout_ms;
    // `0` here would turn every bind to a slow module into an instant failure;
    // "off is not a budget" so refuse the key at parse time. Operators who
    // want a module unreachable should use `enabled: false` instead. The
    // check is per-layer (daemon-wide + per-module) because either alone
    // poisons every affected bind.
    let default_route_bind_relay_timeout_ms = match raw.route_bind_relay_timeout_ms {
        Some(0) => {
            return Err(DaemonConfigError::InvalidValue {
                path: path.to_path_buf(),
                message: ROUTE_BIND_RELAY_ZERO_MESSAGE.to_string(),
            });
        }
        Some(value) => Some(value),
        None => None,
    };
    let modules = raw
        .modules
        .into_iter()
        .map(|(module_id, module)| {
            let health = module
                .health
                .map(|health| parse_health_config(health, path, &module_id))
                .transpose()?
                .unwrap_or_default();
            if let Err(reason) = crate::registry::module_id_path_hazard(&module_id) {
                return Err(DaemonConfigError::InvalidValue {
                    path: path.to_path_buf(),
                    message: format!(
                        "module id '{}' is not usable as a path component ({reason}): \
                         the daemon derives each module's store path from its id",
                        module_id.escape_debug()
                    ),
                });
            }
            // Same rejection at the per-module layer. `Some(0)` from a module
            // is refused even when the daemon-wide value is also Some(0): the
            // failure must name the offending module id so the operator can
            // locate it in the file.
            let per_module_route_bind_relay_timeout_ms = match module.route_bind_relay_timeout_ms {
                Some(0) => {
                    return Err(DaemonConfigError::InvalidValue {
                        path: path.to_path_buf(),
                        message: format!(
                            "module '{module_id}' {ROUTE_BIND_RELAY_ZERO_MESSAGE}",
                            module_id = module_id.escape_debug()
                        ),
                    });
                }
                Some(value) => Some(value),
                None => default_route_bind_relay_timeout_ms,
            };
            Ok(ConfiguredModule {
                module_id,
                program: module.program,
                args: module.args,
                env: module.env.into_iter().collect(),
                enabled: module.enabled,
                reserved: module.reserved,
                reserved_prefixes: module.reserved_prefixes,
                health,
                // Per-module wins; the daemon-wide value is the fallback. `0` is
                // legitimate ("never wait"), so this is `.or`, not `filter+or`.
                drain_timeout_ms: module.drain_timeout_ms.or(default_drain_timeout_ms),
                // Same shape as drain: an explicit per-module value wins over
                // the daemon-wide default. A `0` here is rejected above
                // (see "off is not a budget"), so `None` means "use the
                // daemon-wide value" and `Some(value > 0)` means "use this".
                route_bind_relay_timeout_ms: per_module_route_bind_relay_timeout_ms,
            })
        })
        .collect::<Result<Vec<_>, DaemonConfigError>>()?;

    validate_reserved_prefixes(&modules, path)?;
    validate_reserved_capabilities(&raw.reserved_capabilities, path)?;
    validate_admission_facts_config(
        &modules,
        raw.admission_facts_carrier_module_id.as_deref(),
        raw.admission_facts_targets.as_deref(),
        path,
    )?;

    let storage = raw.storage.map(|s| match s {
        RawStorageConfig::Sqlite { data_home } => StorageConfig::Sqlite {
            data_home: data_home.unwrap_or_else(default_data_home),
        },
    });

    Ok(DaemonConfig {
        path: path.to_path_buf(),
        port: raw.port,
        drain_timeout_ms: default_drain_timeout_ms,
        route_bind_relay_timeout_ms: default_route_bind_relay_timeout_ms,
        modules,
        storage,
        admission_facts_carrier_module_id: raw.admission_facts_carrier_module_id,
        admission_facts_targets: raw.admission_facts_targets,
        reserved_capabilities: raw.reserved_capabilities,
    })
}

fn validate_reserved_capabilities(
    bindings: &BTreeMap<String, String>,
    path: &Path,
) -> Result<(), DaemonConfigError> {
    for (capability, module_id) in bindings {
        if !is_valid_capability_identifier(capability) {
            return Err(DaemonConfigError::InvalidValue {
                path: path.to_path_buf(),
                message: format!(
                    "reserved_capabilities key {:?} is not a valid capability identifier",
                    capability
                ),
            });
        }
        if module_id.trim().is_empty() {
            return Err(DaemonConfigError::InvalidValue {
                path: path.to_path_buf(),
                message: format!(
                    "reserved_capabilities binding for {:?} has an empty module id",
                    capability
                ),
            });
        }
        if let Err(reason) = crate::registry::module_id_path_hazard(module_id) {
            return Err(DaemonConfigError::InvalidValue {
                path: path.to_path_buf(),
                message: format!(
                    "reserved_capabilities binding for {:?} has an unusable module id {:?}: {reason}",
                    capability, module_id
                ),
            });
        }
    }
    Ok(())
}

fn validate_admission_facts_config(
    modules: &[ConfiguredModule],
    carrier_module_id: Option<&str>,
    targets: Option<&[String]>,
    path: &Path,
) -> Result<(), DaemonConfigError> {
    let Some(carrier_module_id) = carrier_module_id else {
        return Ok(());
    };

    let Some(carrier) = modules
        .iter()
        .find(|module| module.module_id == carrier_module_id)
    else {
        return Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: format!(
                "admission_facts_carrier_module_id '{carrier_module_id}' must name a configured module"
            ),
        });
    };
    if !carrier.enabled || !carrier.reserved {
        return Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: format!(
                "admission_facts_carrier_module_id '{carrier_module_id}' must name an enabled reserved module"
            ),
        });
    }

    let Some(targets) = targets else {
        return Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: "admission_facts_targets must be present when an admission facts carrier is configured".to_string(),
        });
    };
    if targets.is_empty() || targets.iter().any(String::is_empty) {
        return Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message:
                "admission_facts_targets must be non-empty and must not contain empty module ids"
                    .to_string(),
        });
    }

    Ok(())
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
        on_degraded: match raw.on_degraded {
            Some(RawHealthAction::Restart) => {
                return Err(DaemonConfigError::InvalidValue {
                    path: path.to_path_buf(),
                    message: format!(
                        "module '{module_id}' health.on_degraded may not be 'restart': a degraded module is slow-but-moving, so restarting it converts transient load into an outage. Use 'report' or 'alert' (Health-Path v2: only total wreckage or reported-unresponsiveness restarts)."
                    ),
                });
            }
            Some(action) => health_action(action),
            None => defaults.on_degraded,
        },
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

    /// The golden fixture is the CONTRACT for data-home resolution: mirror
    /// implementations (cortexkit-store-types `resolve_data_home`,
    /// @cortexkit/store `resolveDataHome`) assert against the same rows, so a
    /// rule change here that skips the fixture breaks THIS test rather than
    /// silently splitting a module's self-resolved path from the descriptor
    /// the daemon serves (the CKCRED Windows divergence, 2026-08).
    /// Env-mutating tests share this lock: cargo runs tests on multiple
    /// threads and the four data-home variables are process-global.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn default_data_home_matches_golden_fixture() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/data_home_resolution.json"))
                .expect("golden parses");
        let vars = ["XDG_DATA_HOME", "APPDATA", "USERPROFILE", "HOME"];
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            vars.iter().map(|v| (*v, env::var_os(v))).collect();
        let platform_matches =
            |p: &str| p == "any" || p == if cfg!(windows) { "windows" } else { "unix" };

        let mut ran = 0usize;
        for case in doc["cases"].as_array().expect("cases array") {
            let name = case["name"].as_str().expect("name");
            if !platform_matches(case["platform"].as_str().expect("platform")) {
                continue;
            }
            for v in vars {
                env::remove_var(v);
            }
            for (k, v) in case["env"].as_object().expect("env map") {
                env::set_var(k, v.as_str().expect("env value"));
            }
            let got = default_data_home();
            assert_eq!(
                got.to_string_lossy(),
                case["expect"].as_str().expect("expect"),
                "golden case '{name}' diverged"
            );
            ran += 1;
        }
        // Vacuity floor: 'any' rows plus this platform's rows must both run.
        assert!(
            ran >= 6,
            "only {ran} golden cases ran; fixture or filter broken"
        );

        for (k, v) in saved {
            match v {
                Some(val) => env::set_var(k, val),
                None => env::remove_var(k),
            }
        }
    }

    #[test]
    fn restart_required_sections_are_the_rescan_cannot_apply_set() {
        assert_eq!(
            RESTART_REQUIRED_SECTIONS,
            &[
                "port",
                "storage",
                "admission_facts_carrier_module_id",
                "admission_facts_targets",
            ]
        );
    }

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
        // Mutating the environment is a process-wide side effect; every test
        // reading or writing the data-home variables serializes on ENV_LOCK
        // (the golden-fixture test above mutates all four variables).
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
    fn path_hazard_module_id_refuses_config_parse() {
        let path = Path::new("/tmp/subc.jsonc");
        let err = parse_doc(
            r#"{ "version": 1, "modules": { "../escape": { "program": "x" } } }"#,
            path,
        )
        .expect_err("separator-bearing module id must refuse");
        let text = format!("{err}");
        assert!(
            text.contains("not usable as a path component"),
            "refusal must name the hazard: {text}"
        );
    }

    #[test]
    fn drain_timeout_resolves_module_over_daemon_over_absent() {
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"
            {
              "version": 1,
              "drain_timeout_ms": 45000,
              "modules": {
                "fast": { "program": "fast", "drain_timeout_ms": 0 },
                "slow": { "program": "slow", "drain_timeout_ms": 120000 },
                "inherits": { "program": "inherits" }
              }
            }
            "#,
            path,
        )
        .unwrap();
        let by_id = |id: &str| {
            config
                .modules
                .iter()
                .find(|m| m.module_id == id)
                .unwrap()
                .drain_timeout_ms
        };
        // Per-module wins, INCLUDING an explicit 0 ("never wait") -- the case a
        // truthiness-shaped resolution would silently replace with the default.
        assert_eq!(by_id("fast"), Some(0));
        assert_eq!(by_id("slow"), Some(120_000));
        // No per-module value: the daemon-wide default flows in at parse time.
        assert_eq!(by_id("inherits"), Some(45_000));
        assert_eq!(config.drain_timeout_ms, Some(45_000));
    }

    #[test]
    fn drain_timeout_absent_everywhere_stays_none_for_builtin_default() {
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"{ "version": 1, "modules": { "m": { "program": "m" } } }"#,
            path,
        )
        .unwrap();
        // None here is load-bearing: it means "use the compiled default", so a
        // future default bump reaches every unconfigured module without a
        // config migration.
        assert_eq!(config.modules[0].drain_timeout_ms, None);
        assert_eq!(config.drain_timeout_ms, None);
    }

    #[test]
    fn route_bind_relay_timeout_resolves_module_over_daemon_over_absent() {
        // Precedence still holds for valid non-zero values. `0` at either
        // layer is rejected by `route_bind_relay_timeout_zero_at_daemon_layer_is_refused`
        // and `route_bind_relay_timeout_zero_at_module_layer_is_refused` below
        // — the asymmetry is deliberate (drain `0` is still accepted; see
        // `drain_timeout_zero_still_parses_for_wedge_bounces`).
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"
            {
              "version": 1,
              "route_bind_relay_timeout_ms": 30000,
              "modules": {
                "tight": { "program": "tight", "route_bind_relay_timeout_ms": 5000 },
                "loose": { "program": "loose", "route_bind_relay_timeout_ms": 60000 },
                "inherits": { "program": "inherits" }
              }
            }
            "#,
            path,
        )
        .unwrap();
        let by_id = |id: &str| {
            config
                .modules
                .iter()
                .find(|m| m.module_id == id)
                .unwrap()
                .route_bind_relay_timeout_ms
        };
        // Per-module wins for every non-zero value.
        assert_eq!(by_id("tight"), Some(5_000));
        assert_eq!(by_id("loose"), Some(60_000));
        // No per-module value: the daemon-wide default flows in at parse time.
        assert_eq!(by_id("inherits"), Some(30_000));
        assert_eq!(config.route_bind_relay_timeout_ms, Some(30_000));
    }

    #[test]
    fn route_bind_relay_timeout_zero_at_daemon_layer_is_refused() {
        let path = Path::new("/tmp/subc.jsonc");
        let err = parse_doc(
            r#"
            {
              "version": 1,
              "route_bind_relay_timeout_ms": 0,
              "modules": { "m": { "program": "m" } }
            }
            "#,
            path,
        )
        .expect_err("a daemon-wide zero budget must refuse parse");
        let text = format!("{err}");
        assert!(
            text.contains("route_bind_relay_timeout_ms"),
            "error must name the offending key: {text}"
        );
        assert!(
            text.contains("enabled: false"),
            "error must name the remedy (enable false): {text}"
        );
    }

    #[test]
    fn route_bind_relay_timeout_zero_at_module_layer_is_refused() {
        let path = Path::new("/tmp/subc.jsonc");
        let err = parse_doc(
            r#"
            {
              "version": 1,
              "modules": {
                "good": { "program": "good" },
                "broken": { "program": "broken", "route_bind_relay_timeout_ms": 0 }
              }
            }
            "#,
            path,
        )
        .expect_err("a per-module zero budget must refuse parse");
        let text = format!("{err}");
        assert!(
            text.contains("route_bind_relay_timeout_ms"),
            "error must name the offending key: {text}"
        );
        assert!(
            text.contains("broken"),
            "error must name the offending module id: {text}"
        );
        assert!(
            text.contains("enabled: false"),
            "error must name the remedy (enable false): {text}"
        );
    }

    #[test]
    fn drain_timeout_zero_still_parses_for_wedge_bounces() {
        // The asymmetry guard: `drain_timeout_ms: 0` is the sanctioned "tear
        // down now" used during a wedge bounce and MUST keep parsing. Anyone
        // later tempted to "fix the inconsistency" between drain and bind by
        // rejecting drain `0` too will break the wedge-bounce path; this
        // test names that contract explicitly.
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"
            {
              "version": 1,
              "drain_timeout_ms": 0,
              "modules": {
                "wedge": { "program": "wedge", "drain_timeout_ms": 0 }
              }
            }
            "#,
            path,
        )
        .expect("drain_timeout_ms: 0 must still parse; wedge-bounce uses it");
        let wedge = config
            .modules
            .iter()
            .find(|m| m.module_id == "wedge")
            .unwrap();
        assert_eq!(wedge.drain_timeout_ms, Some(0));
        assert_eq!(config.drain_timeout_ms, Some(0));
    }

    #[test]
    fn route_bind_relay_timeout_absent_everywhere_stays_none_for_builtin_default() {
        // Backward-compatibility guard: a config that does not mention
        // `route_bind_relay_timeout_ms` at all (the shape every pre-#38 daemon
        // shipped) parses to `None` on both layers, so the bind path keeps
        // its compiled 12s default.
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"{ "version": 1, "modules": { "m": { "program": "m" } } }"#,
            path,
        )
        .unwrap();
        assert_eq!(config.modules[0].route_bind_relay_timeout_ms, None);
        assert_eq!(config.route_bind_relay_timeout_ms, None);
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
    fn reserved_capabilities_accept_unknown_bound_modules_and_refuse_bad_identifiers() {
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"{
                "version": 1,
                "reserved_capabilities": {
                    "credentials-provider/v1": "future-vault"
                },
                "modules": {}
            }"#,
            path,
        )
        .expect("a binding may predate its provider installation");
        assert_eq!(
            config.reserved_capabilities,
            BTreeMap::from([(
                "credentials-provider/v1".to_string(),
                "future-vault".to_string()
            )])
        );

        let error = parse_doc(
            r#"{
                "version": 1,
                "reserved_capabilities": { "Credentials/v1": "vault" },
                "modules": {}
            }"#,
            path,
        )
        .expect_err("reserved capabilities use the capability identifier grammar");
        assert!(error.to_string().contains("reserved_capabilities key"));
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
    fn admission_facts_carrier_requires_non_empty_targets() {
        let missing_targets = parse_doc(
            r#"{
              "version": 1,
              "admission_facts_carrier_module_id": "fed",
              "modules": { "fed": { "program": "fed", "reserved": true } }
            }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        // Pin the message, not just the variant. Every rule in this validator
        // returns InvalidValue, and the guard below rejects an empty list -- so
        // a change that turned a missing list into an empty one would still be
        // refused, by a different rule, and a variant-only assertion could not
        // tell the two apart.
        assert!(
            matches!(&missing_targets, DaemonConfigError::InvalidValue { message, .. }
                if message.contains("must be present")),
            "expected the presence rule, got: {missing_targets:?}"
        );

        let empty_targets = parse_doc(
            r#"{
              "version": 1,
              "admission_facts_carrier_module_id": "fed",
              "admission_facts_targets": [""],
              "modules": { "fed": { "program": "fed", "reserved": true } }
            }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(
            matches!(&empty_targets, DaemonConfigError::InvalidValue { message, .. }
                if message.contains("must be non-empty")),
            "expected the non-empty rule, got: {empty_targets:?}"
        );
    }

    #[test]
    fn admission_facts_carrier_must_be_enabled_reserved_and_configured() {
        for module in [
            r#"{ "program": "fed", "enabled": false, "reserved": true }"#,
            r#"{ "program": "fed", "enabled": true, "reserved": false }"#,
        ] {
            let doc = format!(
                r#"{{
                  "version": 1,
                  "admission_facts_carrier_module_id": "fed",
                  "admission_facts_targets": ["target"],
                  "modules": {{ "fed": {module}, "target": {{ "program": "target" }} }}
                }}"#
            );
            let err = parse_doc(&doc, Path::new("subc.jsonc")).unwrap_err();
            // Pin which refusal fired. Both inputs are also missing nothing
            // else, so without this the neighbouring "must name a configured
            // module" rule would satisfy the assertion if this one were removed.
            assert!(
                matches!(&err, DaemonConfigError::InvalidValue { message, .. }
                    if message.contains("enabled reserved module")),
                "expected the enabled-and-reserved rule, got: {err:?}"
            );
        }

        let absent = parse_doc(
            r#"{
              "version": 1,
              "admission_facts_carrier_module_id": "missing",
              "admission_facts_targets": ["target"],
              "modules": { "target": { "program": "target" } }
            }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(
            matches!(&absent, DaemonConfigError::InvalidValue { message, .. }
                if message.contains("must name a configured module")),
            "expected the configured-module rule, got: {absent:?}"
        );
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
