use std::{
    collections::BTreeMap,
    env,
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use cortexkit_log::Retention;
use serde::Deserialize;
use subc_control::ModuleProtocol;
use subc_jsonc::jsonc_to_json;
use subc_protocol::manifest::is_valid_capability_identifier;

use crate::{
    supervise::{ModuleOverlap, SUBC_SPAWN_ROLE_ENV},
    HealthAction, HealthConfig, ModuleSpec, RestartPolicy,
};

const DAEMON_CONFIG_RELATIVE_PATH: &str = "cortexkit/subc.jsonc";
const SUPPORTED_CONFIG_VERSION: u32 = 1;
pub(crate) const CK_LOG_ENV: &str = "CK_LOG";
pub(crate) const CAPTURE_MAX_FILE_MB_ENV: &str = "__SUBC_CAPTURE_LOG_MAX_FILE_MB";
pub(crate) const CAPTURE_KEEP_ENV: &str = "__SUBC_CAPTURE_LOG_KEEP";
pub(crate) const CAPTURE_MAX_AGE_DAYS_ENV: &str = "__SUBC_CAPTURE_LOG_MAX_AGE_DAYS";
/// The child's own segment retention, read by `cortexkit_log::Config::from_env`.
/// Unlike the `__SUBC_CAPTURE_*` names above these are a real child-process
/// contract and are spawned into the environment.
pub(crate) const CHILD_LOG_MAX_AGE_DAYS_ENV: &str = "CK_LOG_MAX_AGE_DAYS";
pub(crate) const CHILD_LOG_ALARM_SEGMENT_MB_ENV: &str = "CK_LOG_ALARM_SEGMENT_MB";

/// Top-level daemon config sections that rescan cannot apply. The daemon
/// snapshots these sections at start and reports later rescan changes as
/// `restart_required`. Setup intersects this set with sections core
/// configuration would write so a dry-run can flag a restart before the
/// config file exists on disk. Match this enum exhaustively so a new section
/// cannot be added without a comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartRequiredSection {
    Port,
    Storage,
    AdmissionFactsCarrierModuleId,
    AdmissionFactsTargets,
}

impl RestartRequiredSection {
    pub const ALL: [Self; 4] = [
        Self::Port,
        Self::Storage,
        Self::AdmissionFactsCarrierModuleId,
        Self::AdmissionFactsTargets,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Port => "port",
            Self::Storage => "storage",
            Self::AdmissionFactsCarrierModuleId => "admission_facts_carrier_module_id",
            Self::AdmissionFactsTargets => "admission_facts_targets",
        }
    }
}

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

/// Refused at parse time because a zero window and a large one are different
/// settings that look alike in a diff. The crash budget counts restarts inside
/// `window_secs`; with `0`, no restart is ever inside it, so the cap can never
/// be reached and the module restarts forever. That is a real posture, but it
/// is "unlimited restarts", and anyone choosing it must say so by name rather
/// than by writing a zero that reads like "no delay".
const RESTART_WINDOW_ZERO_MESSAGE: &str = "restart.window_secs must be greater than 0 (a zero window holds no crash, so the budget can never be spent; for effectively unlimited restarts set a deliberately large window_secs, and to stop restarting entirely set restart.max_restarts: 0)";

/// Logging policy parsed from `subc.jsonc`.
///
/// `retention` is the rename-rotating policy for the daemon's per-child
/// stderr CAPTURE file (single writer). The daemon's own log and every module's
/// log are date segments under fleet-logging r2, which never rotate; for those
/// only `retention.max_age_days` applies, plus `alarm_segment_mb`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggingConfig {
    pub level: String,
    /// Per-logger levels. Keys are logger names; a key with no dot is taken
    /// as a COMPONENT of the module it is configured on (`perf` on synapse is
    /// `synapse.perf`), so an operator's `subc.jsonc` reads naturally. See
    /// [`LoggingConfig::filter_spec`].
    pub tags: BTreeMap<String, String>,
    pub retention: Retention,
    /// Segment size at which the writer alarms (never truncates).
    pub alarm_segment_mb: u32,
}

impl LoggingConfig {
    /// The `CK_LOG` value for `module_id`. Logger names in `CK_LOG` are
    /// absolute (`synapse.perf=info`), while the config block is written per
    /// module, so a dotless key is prefixed with the module id here. A key
    /// that already starts with `<module_id>.` or contains a dot is passed
    /// verbatim; a key equal to the module id is the root and is also
    /// verbatim. Without this a config `tags: { perf: debug }` would emit
    /// `perf=debug`, which matches no logger on the r2 hierarchy and silently
    /// does nothing.
    pub fn filter_spec(&self, module_id: &str) -> String {
        let mut directives = vec![self.level.clone()];
        directives.extend(self.tags.iter().map(|(logger, level)| {
            if logger == module_id || logger.contains('.') {
                format!("{logger}={level}")
            } else {
                format!("{module_id}.{logger}={level}")
            }
        }));
        directives.join(",")
    }

    pub fn segment_retention(&self) -> cortexkit_log::SegmentRetention {
        cortexkit_log::SegmentRetention {
            max_age_days: self.retention.max_age_days,
            alarm_segment_mb: self.alarm_segment_mb,
        }
    }
}

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
    /// Effective module logging policy. An absent module block inherits the
    /// daemon-wide logging block; when neither exists this stays absent so
    /// `CK_LOG` is genuinely absent from the service-manager-minimal child env.
    pub log: Option<LoggingConfig>,
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
    /// Which wire protocol this module speaks, as declared. Absent in config
    /// means `Subc`, which is what every module written before this key meant.
    pub protocol: ModuleProtocol,
    /// Whether a second process of this module may run beside the first, which
    /// a blue/green swap does. Absent in config means exclusive.
    pub overlap: ModuleOverlap,
    pub health: HealthConfig,
    /// Effective drain budget (ms) for this module's teardown, already resolved
    /// against the daemon-wide default at parse time. `None` = built-in default.
    pub drain_timeout_ms: Option<u64>,
    /// Effective route.bind relay budget (ms) for this module, already resolved
    /// against the daemon-wide default at parse time. `None` = built-in default
    /// (12s). A `0` is refused at parse time at both layers — see
    /// `DaemonConfig::route_bind_relay_timeout_ms` and `ROUTE_BIND_RELAY_ZERO_MESSAGE`.
    pub route_bind_relay_timeout_ms: Option<u64>,
    /// This module's crash-restart budget, fully resolved at parse time: every
    /// absent key of the optional `restart` block falls back to the supervisor
    /// default (3 restarts per 600s, 100ms base backoff, 30s maximum backoff).
    /// Stored resolved rather than as an `Option` so no later layer has to
    /// re-derive the defaults and get them subtly different.
    ///
    /// Read when a module STARTS being supervised (daemon start, or a rescan
    /// that adds the module). Like `drain_timeout_ms`, an edit to this block for
    /// an already-running module is not part of the rescan diff, so it takes
    /// effect on the next daemon start rather than immediately.
    pub restart: RestartPolicy,
}

impl ConfiguredModule {
    pub fn module_spec(&self) -> ModuleSpec {
        let mut env = self.env.clone();
        if let Some(log) = &self.log {
            env.retain(|(key, _)| {
                key != CK_LOG_ENV
                    && key != CAPTURE_MAX_FILE_MB_ENV
                    && key != CAPTURE_KEEP_ENV
                    && key != CAPTURE_MAX_AGE_DAYS_ENV
            });
            env.retain(|(key, _)| {
                key != CHILD_LOG_MAX_AGE_DAYS_ENV && key != CHILD_LOG_ALARM_SEGMENT_MB_ENV
            });
            env.push((CK_LOG_ENV.to_string(), log.filter_spec(&self.module_id)));
            env.push((
                CHILD_LOG_MAX_AGE_DAYS_ENV.to_string(),
                log.retention.max_age_days.to_string(),
            ));
            env.push((
                CHILD_LOG_ALARM_SEGMENT_MB_ENV.to_string(),
                log.alarm_segment_mb.to_string(),
            ));
            // The capture file's own rotation policy. These private entries are
            // supervisor metadata and are removed before spawn: the child never
            // sees them, and the capture sink reads them back at spawn time.
            env.push((
                CAPTURE_MAX_FILE_MB_ENV.to_string(),
                log.retention.max_file_mb.to_string(),
            ));
            env.push((CAPTURE_KEEP_ENV.to_string(), log.retention.keep.to_string()));
            env.push((
                CAPTURE_MAX_AGE_DAYS_ENV.to_string(),
                log.retention.max_age_days.to_string(),
            ));
        }
        ModuleSpec {
            module_id: self.module_id.clone(),
            program: self.program.clone(),
            args: self.args.clone(),
            env,
            reserved: self.reserved,
            reserved_prefixes: self.reserved_prefixes.clone(),
            protocol: self.protocol,
            overlap: self.overlap,
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
    log: Option<RawLoggingConfig>,
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
    #[serde(default)]
    log: Option<RawLoggingConfig>,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default)]
    reserved: bool,
    #[serde(default)]
    reserved_prefixes: Vec<String>,
    /// Read as a raw string rather than a serde enum so an unusable value is
    /// refused as an `InvalidValue` naming the module and the value the operator
    /// typed, instead of a serde variant error that names neither.
    #[serde(default)]
    protocol: Option<String>,
    /// Read as a raw string for the same reason as `protocol`.
    #[serde(default)]
    overlap: Option<String>,
    #[serde(default)]
    health: Option<RawHealthConfig>,
    #[serde(default)]
    drain_timeout_ms: Option<u64>,
    #[serde(default)]
    route_bind_relay_timeout_ms: Option<u64>,
    #[serde(default)]
    restart: Option<RawRestartConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawLoggingConfig {
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    #[serde(default)]
    alarm_segment_mb: Option<u32>,
    #[serde(default)]
    max_file_mb: Option<u32>,
    #[serde(default)]
    keep: Option<u8>,
    #[serde(default)]
    max_age_days: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawRestartConfig {
    #[serde(default)]
    max_restarts: Option<u32>,
    #[serde(default)]
    window_secs: Option<u64>,
    #[serde(default)]
    backoff_ms: Option<u64>,
    #[serde(default)]
    max_backoff_ms: Option<u64>,
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
    default_config_home().join(DAEMON_CONFIG_RELATIVE_PATH)
}

/// The XDG-style CONFIG HOME (`~/.config`, `%APPDATA%`), with no `cortexkit/`
/// tail. This is the AUTHORITY for every module that resolves its own config
/// file: mirrors (`cortexkit-store-types::resolve_config_home`, and any module
/// still carrying a hand copy of this ladder) assert against
/// `tests/golden/config_home_resolution.json` and may not diverge. It is split
/// from `default_config_path` so the mirror and the daemon share one ladder
/// rather than one ladder plus a tail that each copy re-appends differently --
/// the daemon appends `cortexkit/subc.jsonc`, a module appends
/// `cortexkit/<its file>`, and a copy that bakes the tail in cannot be reused.
///
/// Resolution: `XDG_CONFIG_HOME` → `APPDATA` (Windows) → `USERPROFILE` +
/// `AppData\Roaming` (Windows) → `HOME/.config` → `.config` relative.
/// Empty values count as unset. Mirrors the data-home ladder exactly except for
/// the per-platform tails (`.local/share` there, `.config` here).
///
/// A RELATIVE result means one of two things and the resolver does not say
/// which: no home variable was set (the final rung), or `XDG_CONFIG_HOME` was
/// itself relative (honoured as-is, golden-pinned). Either way the path resolves
/// against the caller's cwd, which is a true answer about a directory nobody
/// chose. Callers that must be fail-closed check `is_absolute()` and refuse;
/// the daemon does so for the storage descriptor it serves (`parse_doc`).
pub fn default_config_home() -> PathBuf {
    if let Some(config_home) = non_empty_os_var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home);
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
        return PathBuf::from(home).join(".config");
    }

    PathBuf::from(".config")
}

pub fn load(path: impl AsRef<Path>) -> Result<Option<DaemonConfig>, DaemonConfigError> {
    let path = path.as_ref();
    let Some(doc) = read_config_doc(path)? else {
        return Ok(None);
    };
    parse_doc(&doc, path).map(Some)
}

/// Loads only the daemon-wide logging block for tracing initialization.
///
/// The daemon installs its global subscriber before bootstrap parses the full
/// configuration. A malformed full config is still reported by bootstrap after
/// the file sink is live; this early read only chooses its filter and retention.
pub fn load_logging(path: impl AsRef<Path>) -> Result<Option<LoggingConfig>, DaemonConfigError> {
    let path = path.as_ref();
    let Some(doc) = read_config_doc(path)? else {
        return Ok(None);
    };
    let json = jsonc_to_json(&doc).map_err(|message| DaemonConfigError::InvalidJsonc {
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
    raw.log
        .map(|log| parse_logging_config(log, path, "daemon log"))
        .transpose()
}

/// Create the daemon run directory at 0700 if absent, and tighten it if wider.
///
/// WHY A SEPARATE STEP RATHER THAN A MODE ON THE CREATOR. Several things create
/// this directory and none of them owns it: the log sink's `create_dir_all`
/// (0777 & ~umask, so 0755 on a default desk), the terminal journal, and the
/// connection-file writer -- which DOES build its parents at 0700, but returns
/// early when the directory already exists, because an existing directory keeps
/// its mode. So the first creator to run decides the mode for every later one,
/// and on this fleet that was the log sink.
///
/// WHAT THE BIT COSTS, stated so nobody over- or under-reads it: the connection
/// secret inside is written 0600 and was never readable by another account. A
/// world-listable run directory leaks the MAP -- which modules are live and what
/// their connection files are named -- not the key. It is worth closing anyway
/// because the map is reconnaissance and costs nothing to withhold. (Found by
/// prefrontal's campaign-rig isolation probe, 2026-09-20, on a real desk.)
///
/// TIGHTENING IS BEST-EFFORT AND NEVER FATAL. The daemon does not own every
/// deployment: a directory it cannot chmod belongs to someone else, and refusing
/// to boot over a permission bit would trade a reconnaissance leak for an
/// outage. The caller logs what it could not do.
///
/// A run directory that cannot be resolved (see [`daemon_run_dir`]) is reported
/// as an `InvalidInput` I/O error rather than created under the working
/// directory.
pub fn ensure_daemon_run_dir_private() -> Result<PathBuf, io::Error> {
    let path = daemon_run_dir()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    ensure_directory_private(&path)?;
    Ok(path)
}

/// The policy half, taking the directory so a test drives a real one without
/// touching the process environment (this crate forbids unsafe, and `set_var` is
/// unsafe in this edition -- which is the better outcome: the seam is a parameter
/// rather than a global the test has to fight).
#[cfg(unix)]
fn ensure_directory_private(path: &Path) -> Result<(), io::Error> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        return Ok(());
    }
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Windows has no mode bits to tighten; the directory is created on first use.
#[cfg(not(unix))]
fn ensure_directory_private(path: &Path) -> Result<(), io::Error> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

/// Existing per-user daemon run directory (`<data-home>/cortexkit/run`).
///
/// Refuses a relative data home (HOME and XDG_DATA_HOME both unset, or a
/// relative XDG_DATA_HOME) instead of resolving it against the working
/// directory. Resolving it there once wrote a stray `.local/` tree into a crate
/// directory and dirtied a release build: the run directory holds the
/// connection file, the terminal journal and the daemon's logs, so it must not
/// depend on where a process happened to be started. The storage data home is
/// refused at config parse for the same reason.
pub fn daemon_run_dir() -> Result<PathBuf, DaemonRunDirError> {
    daemon_run_dir_from(default_data_home())
}

/// The policy half of [`daemon_run_dir`], taking the data home as a parameter so
/// a test can drive both outcomes without touching the process environment.
fn daemon_run_dir_from(data_home: PathBuf) -> Result<PathBuf, DaemonRunDirError> {
    if !data_home.is_absolute() {
        return Err(DaemonRunDirError::RelativeDataHome { data_home });
    }
    Ok(data_home.join("cortexkit").join("run"))
}

/// Why the daemon run directory could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonRunDirError {
    /// The data home resolved to a relative path, which would place the run
    /// directory under whatever directory the process was started from.
    RelativeDataHome { data_home: PathBuf },
}

impl fmt::Display for DaemonRunDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RelativeDataHome { data_home } => write!(
                f,
                "cannot resolve the daemon run directory: the data home `{}` is relative, \
                 so it would land under the current working directory; {}",
                data_home.display(),
                DATA_HOME_REMEDY
            ),
        }
    }
}

impl std::error::Error for DaemonRunDirError {}

/// Which environment variables make the data home absolute on this platform.
#[cfg(windows)]
const DATA_HOME_REMEDY: &str =
    "set XDG_DATA_HOME to an absolute path, or set APPDATA, USERPROFILE or HOME";
#[cfg(not(windows))]
const DATA_HOME_REMEDY: &str = "set XDG_DATA_HOME to an absolute path, or set HOME";

fn read_config_doc(path: &Path) -> Result<Option<String>, DaemonConfigError> {
    match fs::read_to_string(path) {
        Ok(doc) => Ok(Some(doc)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(DaemonConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
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

    let daemon_logging = raw
        .log
        .map(|log| parse_logging_config(log, path, "daemon log"))
        .transpose()?;
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
            let protocol = parse_module_protocol(module.protocol.as_deref(), path, &module_id)?;
            let overlap = parse_module_overlap(module.overlap.as_deref(), path, &module_id)?;
            // The spawn role is set by the supervisor on a swap candidate and
            // nowhere else; a configured value would put the long swap warm-up
            // budget on every plain restart, where callers wait on it.
            if module.env.contains_key(SUBC_SPAWN_ROLE_ENV) {
                return Err(DaemonConfigError::InvalidValue {
                    path: path.to_path_buf(),
                    message: format!(
                        "module '{module_id}' sets {SUBC_SPAWN_ROLE_ENV} in env; that variable is set by the supervisor on a swap candidate only and cannot be configured",
                        module_id = module_id.escape_debug()
                    ),
                });
            }
            // A reserved module is one only the daemon-spawned process may
            // REGISTER as, enforced by matching a launch nonce in its HELLO. A
            // module that speaks no subc wire sends no HELLO, so the gate has
            // nothing to check and the pairing states an intent the daemon
            // cannot carry out. Refusing at parse is better than accepting a
            // security-looking declaration that protects nothing.
            if protocol == ModuleProtocol::None && module.reserved {
                return Err(DaemonConfigError::InvalidValue {
                    path: path.to_path_buf(),
                    message: format!(
                        "module '{module_id}' sets reserved: true with protocol: \"none\"; \
                         reserved is enforced on the module's HELLO and a protocol: \"none\" \
                         module never registers, so the reservation could never be checked",
                        module_id = module_id.escape_debug()
                    ),
                });
            }
            let restart = parse_restart_config(module.restart, path, &module_id)?;
            let log = module
                .log
                .map(|log| parse_logging_config(log, path, &format!("module '{module_id}' log")))
                .transpose()?
                .or_else(|| daemon_logging.clone());
            Ok(ConfiguredModule {
                module_id,
                program: module.program,
                args: module.args,
                env: module.env.into_iter().collect(),
                log,
                enabled: module.enabled,
                reserved: module.reserved,
                reserved_prefixes: module.reserved_prefixes,
                protocol,
                overlap,
                health,
                // Per-module wins; the daemon-wide value is the fallback. `0` is
                // legitimate ("never wait"), so this is `.or`, not `filter+or`.
                drain_timeout_ms: module.drain_timeout_ms.or(default_drain_timeout_ms),
                // Same shape as drain: an explicit per-module value wins over
                // the daemon-wide default. A `0` here is rejected above
                // (see "off is not a budget"), so `None` means "use the
                // daemon-wide value" and `Some(value > 0)` means "use this".
                route_bind_relay_timeout_ms: per_module_route_bind_relay_timeout_ms,
                restart,
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

    let storage = raw
        .storage
        .map(|s| match s {
            RawStorageConfig::Sqlite { data_home } => {
                let data_home = data_home.unwrap_or_else(default_data_home);
                // A relative data home is served to every module in its storage
                // descriptor and resolves against each module's own cwd, so one
                // daemon would hand out N different directories while every
                // module's gate stays green. The resolver returns a relative
                // path when no home variable is set (golden-pinned) or when an
                // operator set XDG_DATA_HOME to one; both are refused here rather
                // than in the resolver, because the resolver's contract is shared
                // with modules that may legitimately tolerate it.
                if !data_home.is_absolute() {
                    return Err(DaemonConfigError::InvalidValue {
                        path: path.to_path_buf(),
                        message: format!(
                            "storage data home resolved to the relative path {} \
                             (no absolute XDG_DATA_HOME, APPDATA, USERPROFILE, or HOME \
                             in the daemon's environment); refusing to serve a \
                             cwd-relative storage descriptor to modules. Set \
                             XDG_DATA_HOME or HOME to an absolute path, or set \
                             storage.data_home in this file.",
                            data_home.display()
                        ),
                    });
                }
                Ok(StorageConfig::Sqlite { data_home })
            }
        })
        .transpose()?;

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

fn parse_logging_config(
    raw: RawLoggingConfig,
    path: &Path,
    owner: &str,
) -> Result<LoggingConfig, DaemonConfigError> {
    fn valid_level(level: &str) -> bool {
        matches!(level, "off" | "error" | "warn" | "info" | "debug" | "trace")
    }

    let level = raw.level.unwrap_or_else(|| "info".to_string());
    if !valid_level(&level) {
        return Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: format!(
                "{owner}.level must be one of off, error, warn, info, debug, trace; got {level:?}"
            ),
        });
    }
    for (tag, tag_level) in &raw.tags {
        // A logger name is dotted segments of [a-z][a-z0-9-]*: the same
        // grammar cortexkit-log renders and filters on. Anything else would
        // pass through CK_LOG and be refused there, one process away from the
        // config that caused it.
        let well_formed = !tag.is_empty()
            && tag.split('.').all(|segment| {
                let mut chars = segment.chars();
                matches!(chars.next(), Some('a'..='z'))
                    && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
            });
        if !well_formed {
            return Err(DaemonConfigError::InvalidValue {
                path: path.to_path_buf(),
                message: format!(
                    "{owner}.tags key {tag:?} is not a logger name (dotted segments of [a-z][a-z0-9-]*)"
                ),
            });
        }
        if !valid_level(tag_level) {
            return Err(DaemonConfigError::InvalidValue {
                path: path.to_path_buf(),
                message: format!(
                    "{owner}.tags.{tag} must be one of off, error, warn, info, debug, trace; got {tag_level:?}"
                ),
            });
        }
    }

    let defaults = Retention::default();
    let retention = Retention {
        max_file_mb: raw.max_file_mb.unwrap_or(defaults.max_file_mb),
        keep: raw.keep.unwrap_or(defaults.keep),
        max_age_days: raw.max_age_days.unwrap_or(defaults.max_age_days),
    };
    if retention.max_file_mb == 0 {
        return Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: format!("{owner}.max_file_mb must be greater than 0"),
        });
    }

    let alarm_segment_mb = raw
        .alarm_segment_mb
        .unwrap_or(cortexkit_log::SegmentRetention::default().alarm_segment_mb);
    if alarm_segment_mb == 0 {
        return Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: format!("{owner}.alarm_segment_mb must be greater than 0"),
        });
    }

    Ok(LoggingConfig {
        level,
        tags: raw.tags,
        retention,
        alarm_segment_mb,
    })
}

/// Resolve a module's declared `protocol` key.
///
/// Absent and `"subc"` are the SAME answer on purpose: a config written before
/// this key existed meant "a subc module", so there is no third state for
/// "unspecified" to drift into. Anything else is refused with the value quoted,
/// because the alternative -- falling back to `subc` for a typo like `"non"` --
/// silently restores the exact supervision behaviour the operator was trying to
/// turn off.
fn parse_module_protocol(
    raw: Option<&str>,
    path: &Path,
    module_id: &str,
) -> Result<ModuleProtocol, DaemonConfigError> {
    match raw {
        None | Some("subc") => Ok(ModuleProtocol::Subc),
        Some("none") => Ok(ModuleProtocol::None),
        // `{other:?}` quotes and escapes the operator's own bytes, so a value
        // carrying control characters cannot rewrite the terminal of whoever
        // reads the refusal.
        Some(other) => Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: format!(
                "module '{module_id}' declares protocol {other:?}; supported values are \
                 \"subc\" (the default when the key is absent) and \"none\"",
                module_id = module_id.escape_debug(),
            ),
        }),
    }
}

/// Resolve a module's declared `overlap` key. Absent means `"exclusive"`,
/// and an unknown value is refused rather than read as either: a typo that
/// became `"safe"` would let a swap run two processes on a single-writer store.
fn parse_module_overlap(
    raw: Option<&str>,
    path: &Path,
    module_id: &str,
) -> Result<ModuleOverlap, DaemonConfigError> {
    match raw {
        None | Some("exclusive") => Ok(ModuleOverlap::Exclusive),
        Some("safe") => Ok(ModuleOverlap::Safe),
        Some(other) => Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: format!(
                "module '{module_id}' declares overlap {other:?}; supported values are \
                 \"exclusive\" (the default when the key is absent) and \"safe\"",
                module_id = module_id.escape_debug(),
            ),
        }),
    }
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

/// Resolve one module's `restart` block against the supervisor defaults.
///
/// Every key is optional and independent: a config that sets only
/// `window_secs` keeps the default cap and backoff, and a config with no
/// `restart` block at all gets exactly the policy the daemon used before the
/// block existed.
fn parse_restart_config(
    raw: Option<RawRestartConfig>,
    path: &Path,
    module_id: &str,
) -> Result<RestartPolicy, DaemonConfigError> {
    let defaults = RestartPolicy::default();
    let Some(raw) = raw else {
        return Ok(defaults);
    };

    let window = match raw.window_secs {
        Some(0) => {
            return Err(DaemonConfigError::InvalidValue {
                path: path.to_path_buf(),
                message: format!(
                    "module '{module_id}' {RESTART_WINDOW_ZERO_MESSAGE}",
                    module_id = module_id.escape_debug()
                ),
            });
        }
        Some(secs) => Duration::from_secs(secs),
        None => defaults.window,
    };
    let backoff = raw
        .backoff_ms
        .map(Duration::from_millis)
        .unwrap_or(defaults.backoff);
    let max_backoff = raw
        .max_backoff_ms
        .map(Duration::from_millis)
        .unwrap_or(defaults.max_backoff);
    if max_backoff < backoff {
        return Err(DaemonConfigError::InvalidValue {
            path: path.to_path_buf(),
            message: format!(
                "module '{}' restart.max_backoff_ms must be greater than or equal to restart.backoff_ms (max_backoff_ms={max_backoff:?}, backoff_ms={backoff:?})",
                module_id.escape_debug()
            ),
        });
    }

    Ok(RestartPolicy {
        // `0` is a deliberate posture here ("never replace this module"), unlike
        // the window, so it is accepted as written.
        max_restarts: raw.max_restarts.unwrap_or(defaults.max_restarts),
        backoff,
        max_backoff,
        window,
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
pub(crate) fn default_data_home() -> PathBuf {
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

#[cfg(all(test, unix))]
mod run_dir_privacy_tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use subc_test_support::TestTempDir;

    /// Both arms of the thing that actually bit: a directory this code CREATES,
    /// and one it INHERITS from another creator. The second is the real case --
    /// every desk in the fleet already had a 0755 run directory made by the log
    /// sink, so a fix that only sets the mode at creation would have changed
    /// nothing anywhere it mattered.
    #[test]
    fn run_dir_is_created_private_and_an_inherited_wide_one_is_tightened() {
        let temp = TestTempDir::new("subc-run-dir-privacy");
        let created = temp.path().join("cortexkit").join("run");
        super::ensure_directory_private(&created).expect("create run dir");
        let mode = fs::metadata(&created)
            .expect("stat created")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "observable a run directory this code creates must be 0700, got {mode:o}"
        );

        // Now the inherited case: widen it the way create_dir_all would have.
        fs::set_permissions(&created, fs::Permissions::from_mode(0o755)).expect("widen");
        let widened = fs::metadata(&created)
            .expect("stat widened")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            widened, 0o755,
            "observable the fixture must actually be wide before the tighten"
        );

        super::ensure_directory_private(&created).expect("tighten run dir");
        let mode = fs::metadata(&created)
            .expect("stat tightened")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "observable an inherited group- or world-readable run directory must be tightened to 0700, got {mode:o}"
        );
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

    /// A path that is absolute on the platform running the test. `/data` is
    /// relative on Windows (no drive letter), which is not a bug in the resolver
    /// but a bug in a test that assumes POSIX absoluteness -- the relative-home
    /// refusal exposed three such tests on the Windows leg.
    fn abs(posix: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", posix.replace('/', "\\")))
        } else {
            PathBuf::from(posix)
        }
    }

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

    /// A relative data home is what `default_data_home` returns when HOME and
    /// XDG_DATA_HOME are both unset (the golden fixture pins `.local/share`), or
    /// when XDG_DATA_HOME itself is relative. Either must be refused, never
    /// joined onto the working directory.
    #[test]
    fn daemon_run_dir_refuses_a_relative_data_home_and_names_the_variables() {
        for data_home in [PathBuf::from(".local/share"), PathBuf::from("relative-xdg")] {
            let error = daemon_run_dir_from(data_home.clone())
                .expect_err("a relative data home must be refused");
            assert_eq!(
                error,
                DaemonRunDirError::RelativeDataHome {
                    data_home: data_home.clone()
                }
            );
            let message = error.to_string();
            assert!(
                message.contains("XDG_DATA_HOME") && message.contains("HOME"),
                "the refusal must name the variables to set: {message}"
            );
        }
    }

    #[test]
    fn daemon_run_dir_under_an_absolute_data_home_is_cortexkit_run() {
        let data_home = env::temp_dir().join("subc-run-dir-probe").join("data");
        assert!(data_home.is_absolute());
        assert_eq!(
            daemon_run_dir_from(data_home.clone()),
            Ok(data_home.join("cortexkit").join("run"))
        );
    }

    /// Same harness as the data-home golden, over the config-home ladder. The two
    /// fixtures share a row shape on purpose: a divergence between the ladders
    /// (one honouring a variable the other does not) is exactly the class that
    /// produced the doubled-path store defect, and a shared harness makes it
    /// visible as a fixture diff rather than as a runtime surprise.
    #[test]
    fn default_config_home_matches_golden_fixture() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/config_home_resolution.json"))
                .expect("golden parses");
        let vars = ["XDG_CONFIG_HOME", "APPDATA", "USERPROFILE", "HOME"];
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
            let got = default_config_home();
            assert_eq!(
                got.to_string_lossy(),
                case["expect"].as_str().expect("expect"),
                "golden case '{name}' diverged"
            );
            ran += 1;
        }
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

    /// A relative storage data home is refused at parse rather than served.
    /// Both ways a relative path arises are covered: an explicit relative
    /// `storage.data_home` in the file, and the resolver's own fall-through when
    /// no home variable is set. The control proves the guard is on the VALUE and
    /// not on the presence of the key: the same document with an absolute home
    /// parses.
    #[test]
    fn relative_storage_data_home_is_refused_at_parse() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = Path::new("/golden/subc.jsonc");

        // Arm 1: explicit relative value in the file.
        let doc =
            r#"{ "version": 1, "storage": { "backend": "sqlite", "data_home": "relative/home" } }"#;
        let err = parse_doc(doc, path).expect_err("relative data_home must refuse");
        assert!(
            matches!(&err, DaemonConfigError::InvalidValue { message, .. }
                if message.contains("relative path relative/home")),
            "wrong refusal: {err:?}"
        );

        // Arm 2: the resolver's fall-through, with every home variable cleared.
        let vars = ["XDG_DATA_HOME", "APPDATA", "USERPROFILE", "HOME"];
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            vars.iter().map(|v| (*v, env::var_os(v))).collect();
        for v in vars {
            env::remove_var(v);
        }
        let doc = r#"{ "version": 1, "storage": { "backend": "sqlite" } }"#;
        let err = parse_doc(doc, path).expect_err("no home in env must refuse");
        assert!(
            matches!(&err, DaemonConfigError::InvalidValue { message, .. }
                if message.contains("no absolute XDG_DATA_HOME")),
            "wrong refusal: {err:?}"
        );

        // Control: an absolute value parses -- the guard is on the value. The
        // path must be absolute ON THIS PLATFORM; `/abs/home` is relative on
        // Windows and would make the control refuse for the wrong reason.
        let want = abs("/abs/home");
        let doc = format!(
            r#"{{ "version": 1, "storage": {{ "backend": "sqlite", "data_home": {} }} }}"#,
            serde_json::to_string(&want).expect("json path")
        );
        let cfg = parse_doc(&doc, path).expect("absolute data_home parses");
        assert!(matches!(
            cfg.storage,
            Some(StorageConfig::Sqlite { ref data_home }) if *data_home == want
        ));

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
            RestartRequiredSection::ALL.map(RestartRequiredSection::label),
            [
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
            &format!(
                r#"{{ "version": 1, "storage": {{ "backend": "sqlite", "data_home": {} }} }}"#,
                serde_json::to_string(&abs("/data")).expect("json path")
            ),
            Path::new("/tmp/subc.jsonc"),
        )
        .expect("parse");
        assert_eq!(
            config.storage,
            Some(StorageConfig::Sqlite {
                data_home: abs("/data")
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
        std::env::set_var("XDG_DATA_HOME", abs("/forced/data/home"));
        let config = parse_doc(
            r#"{ "version": 1, "storage": { "backend": "sqlite" } }"#,
            Path::new("/tmp/subc.jsonc"),
        )
        .expect("parse");
        std::env::remove_var("XDG_DATA_HOME");
        assert_eq!(
            config.storage,
            Some(StorageConfig::Sqlite {
                data_home: abs("/forced/data/home")
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
    fn log_tag_keys_must_be_logger_names_and_the_error_names_the_key() {
        let path = Path::new("/tmp/subc.jsonc");
        for bad in ["Perf", "a b", "perf.", ".perf", "gc..walk", "a=b"] {
            let doc = format!(
                r#"{{ "version": 1, "modules": {{ "m": {{ "program": "m", "log": {{ "tags": {{ "{bad}": "debug" }} }} }} }} }}"#
            );
            let err = parse_doc(&doc, path).expect_err(bad);
            let text = format!("{err}");
            assert!(
                text.contains(&format!("{bad:?}")),
                "must name the key: {text}"
            );
            assert!(
                text.contains("logger name"),
                "must say what a key is: {text}"
            );
        }
        // Control: dotted, hyphenated, root-equal keys are all fine.
        let ok = parse_doc(
            r#"{ "version": 1, "modules": { "m": { "program": "m", "log": { "tags": { "perf": "debug", "gc.walk": "trace", "m": "error", "a-b": "info" } } } } }"#,
            path,
        );
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn log_filter_spec_prefixes_bare_keys_with_the_module_and_passes_absolute_ones() {
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"{ "version": 1, "modules": { "synapse": { "program": "s", "log": { "level": "warn", "tags": { "perf": "debug", "gc.walk": "trace", "synapse": "error", "other.x": "info" } } } } }"#,
            path,
        )
        .unwrap();
        let log = config.modules[0].log.as_ref().unwrap();
        // BTreeMap order: gc.walk, other.x, perf, synapse.
        assert_eq!(
            log.filter_spec("synapse"),
            "warn,gc.walk=trace,other.x=info,synapse.perf=debug,synapse=error"
        );
    }

    #[test]
    fn log_alarm_segment_mb_defaults_to_the_crate_default_and_refuses_zero() {
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"{ "version": 1, "modules": { "m": { "program": "m", "log": { "level": "info" } } } }"#,
            path,
        )
        .unwrap();
        assert_eq!(
            config.modules[0].log.as_ref().unwrap().alarm_segment_mb,
            cortexkit_log::SegmentRetention::default().alarm_segment_mb
        );
        let err = parse_doc(
            r#"{ "version": 1, "modules": { "m": { "program": "m", "log": { "alarm_segment_mb": 0 } } } }"#,
            path,
        )
        .expect_err("zero alarm must refuse");
        assert!(format!("{err}").contains("alarm_segment_mb"));
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

    /// The shape every config in the field has today: no `restart` block at
    /// all. It must keep parsing, and it must land on the exact policy the
    /// daemon used before the block existed -- all three numbers asserted, so
    /// that quietly changing one is a failing test rather than a fleet-wide
    /// behaviour change nobody configured.
    #[test]
    fn a_config_without_a_restart_block_keeps_the_supervisor_defaults() {
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"{ "version": 1, "modules": { "m": { "program": "m" } } }"#,
            path,
        )
        .unwrap();
        assert_eq!(config.modules[0].restart.max_restarts, 3);
        assert_eq!(config.modules[0].restart.window, Duration::from_secs(600));
        assert_eq!(
            config.modules[0].restart.backoff,
            Duration::from_millis(100)
        );
        assert_eq!(
            config.modules[0].restart.max_backoff,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn a_restart_block_resolves_each_key_independently() {
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"
            {
              "version": 1,
              "modules": {
                "all": {
                  "program": "all",
                  "restart": { "max_restarts": 5, "window_secs": 60, "backoff_ms": 250, "max_backoff_ms": 5000 }
                },
                "window-only": {
                  "program": "window-only",
                  "restart": { "window_secs": 7200 }
                },
                "never": {
                  "program": "never",
                  "restart": { "max_restarts": 0 }
                }
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
                .restart
        };

        let all = by_id("all");
        assert_eq!(all.max_restarts, 5);
        assert_eq!(all.window, Duration::from_secs(60));
        assert_eq!(all.backoff, Duration::from_millis(250));
        assert_eq!(all.max_backoff, Duration::from_secs(5));

        // A module that only widens its window keeps the default cap and
        // backoff: the keys do not travel as a set.
        let window_only = by_id("window-only");
        assert_eq!(window_only.max_restarts, 3);
        assert_eq!(window_only.window, Duration::from_secs(7_200));
        assert_eq!(window_only.backoff, Duration::from_millis(100));
        assert_eq!(window_only.max_backoff, Duration::from_secs(30));

        // `max_restarts: 0` is a posture, not a mistake: never replace this
        // module. Unlike a zero window, it is accepted as written.
        assert_eq!(by_id("never").max_restarts, 0);
    }

    /// A zero window makes the budget unspendable, which is the opposite of a
    /// tight limit and looks almost identical in a diff. Refuse it by name so
    /// the operator writes what they meant.
    #[test]
    fn restart_window_zero_is_refused_by_name() {
        let path = Path::new("/tmp/subc.jsonc");
        let err = parse_doc(
            r#"
            {
              "version": 1,
              "modules": {
                "good": { "program": "good" },
                "broken": { "program": "broken", "restart": { "window_secs": 0 } }
              }
            }
            "#,
            path,
        )
        .expect_err("a zero crash window must refuse parse");
        assert!(
            matches!(err, DaemonConfigError::InvalidValue { .. }),
            "a zero window is an invalid value, not a parse failure: {err:?}"
        );
        let text = format!("{err}");
        assert!(
            text.contains("restart.window_secs"),
            "error must name the offending key: {text}"
        );
        assert!(
            text.contains("broken"),
            "error must name the offending module id: {text}"
        );
        assert!(
            text.contains("max_restarts: 0"),
            "error must name the setting that actually stops restarts: {text}"
        );
    }

    #[test]
    fn restart_max_backoff_below_backoff_is_refused_by_name() {
        let path = Path::new("/tmp/subc.jsonc");
        let err = parse_doc(
            r#"
            {
              "version": 1,
              "modules": {
                "broken": {
                  "program": "broken",
                  "restart": { "backoff_ms": 1000, "max_backoff_ms": 999 }
                }
              }
            }
            "#,
            path,
        )
        .expect_err("a maximum below the base backoff must refuse parse");
        assert!(
            matches!(err, DaemonConfigError::InvalidValue { .. }),
            "an invalid restart bound must be an InvalidValue: {err:?}"
        );
        let text = format!("{err}");
        assert!(
            text.contains("restart.max_backoff_ms"),
            "error must name max_backoff_ms: {text}"
        );
        assert!(
            text.contains("restart.backoff_ms"),
            "error must name backoff_ms: {text}"
        );
        assert!(
            text.contains("broken"),
            "error must name the offending module id: {text}"
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

    /// The three accepted shapes, and the one that matters is that two of them
    /// are THE SAME ANSWER. A config written before this key existed and a
    /// config that spells out `"subc"` must produce an identical module, or the
    /// key would have quietly introduced a third state for every module in every
    /// deployed config file.
    #[test]
    fn an_absent_protocol_key_and_an_explicit_subc_are_the_same_module() {
        let parse = |module_body: &str| {
            parse_doc(
                &format!(
                    r#"{{
                      "version": 1,
                      "modules": {{ "aft": {{ "program": "aft"{module_body} }} }}
                    }}"#
                ),
                Path::new("subc.jsonc"),
            )
            .expect("module parses")
            .modules
            .remove(0)
        };

        let absent = parse("");
        let explicit = parse(r#", "protocol": "subc""#);
        let none = parse(r#", "protocol": "none""#);

        assert_eq!(absent.protocol, ModuleProtocol::Subc);
        assert_eq!(explicit.protocol, ModuleProtocol::Subc);
        assert_eq!(
            absent, explicit,
            "an absent protocol key must produce exactly the module an explicit subc does"
        );
        assert_eq!(none.protocol, ModuleProtocol::None);
        // The declaration has to survive into what the supervisor is handed;
        // parsing it into a field nothing reads would leave every behaviour
        // gated on it unreachable.
        assert_eq!(none.module_spec().protocol, ModuleProtocol::None);
    }

    /// `overlap` defaults to exclusive, `"safe"` opts in and reaches the spec
    /// the supervisor is handed, and anything else is refused rather than read
    /// as either value.
    #[test]
    fn overlap_defaults_to_exclusive_and_only_safe_opts_in() {
        let parse = |module_body: &str| {
            parse_doc(
                &format!(
                    r#"{{
                      "version": 1,
                      "modules": {{ "aft": {{ "program": "aft"{module_body} }} }}
                    }}"#
                ),
                Path::new("subc.jsonc"),
            )
        };

        let absent = parse("").unwrap().modules.remove(0);
        assert_eq!(absent.overlap, ModuleOverlap::Exclusive);
        assert_eq!(absent.module_spec().overlap, ModuleOverlap::Exclusive);
        let safe = parse(r#", "overlap": "safe""#).unwrap().modules.remove(0);
        assert_eq!(safe.module_spec().overlap, ModuleOverlap::Safe);
        let typo = parse(r#", "overlap": "sfae""#).expect_err("an unknown overlap is refused");
        assert!(typo.to_string().contains("sfae"), "{typo}");
    }

    /// The spawn role is the supervisor's to set on a swap candidate. A
    /// configured value would reach every plain spawn and make the module pick
    /// its long swap warm-up budget while callers wait on a restart.
    #[test]
    fn the_spawn_role_is_refused_as_a_configured_env_key() {
        let error = parse_doc(
            r#"{
              "version": 1,
              "modules": { "aft": { "program": "aft", "env": { "SUBC_SPAWN_ROLE": "swap_candidate" } } }
            }"#,
            Path::new("subc.jsonc"),
        )
        .expect_err("SUBC_SPAWN_ROLE must not be configurable");
        assert!(
            matches!(error, DaemonConfigError::InvalidValue { .. }),
            "expected InvalidValue, got {error:?}"
        );
        assert!(error.to_string().contains("SUBC_SPAWN_ROLE"), "{error}");
    }

    /// An unusable value is refused WITH THE VALUE IN THE MESSAGE. Falling back
    /// to `subc` on a typo would restore the exact supervision the operator was
    /// trying to turn off -- health probing, restart-on-silence, SIGKILL
    /// teardown -- and the config file would still read as if it had been
    /// applied.
    #[test]
    fn an_unsupported_protocol_value_is_refused_by_name() {
        let error = parse_doc(
            r#"{
              "version": 1,
              "modules": { "nats": { "program": "nats-server", "protocol": "grpc" } }
            }"#,
            Path::new("subc.jsonc"),
        )
        .expect_err("an unknown protocol must not fall back to a default");

        assert!(
            matches!(error, DaemonConfigError::InvalidValue { .. }),
            "expected InvalidValue, got {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("grpc"),
            "the refusal must name the offending value: {message}"
        );
        assert!(
            message.contains("nats"),
            "the refusal must name the module so it can be found in the file: {message}"
        );
    }

    /// `reserved` is enforced on a module's HELLO. A module that speaks no subc
    /// wire never sends one, so the pair declares a protection that could never
    /// be applied -- worse than no protection, because the config file states it.
    #[test]
    fn reserved_true_with_protocol_none_is_refused_with_the_reason() {
        let error = parse_doc(
            r#"{
              "version": 1,
              "modules": {
                "nats": { "program": "nats-server", "protocol": "none", "reserved": true }
              }
            }"#,
            Path::new("subc.jsonc"),
        )
        .expect_err("a reservation that can never be checked must not parse");

        assert!(
            matches!(error, DaemonConfigError::InvalidValue { .. }),
            "expected InvalidValue, got {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("nats") && message.contains("reserved"),
            "the refusal must name the module and the offending key: {message}"
        );
        assert!(
            message.contains("HELLO") || message.contains("never registers"),
            "the refusal must say WHY the pair cannot work: {message}"
        );
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
