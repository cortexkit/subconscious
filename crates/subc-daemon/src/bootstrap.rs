use std::{
    collections::BTreeMap,
    env,
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use fs4::{FileExt, TryLockError};
use subc_protocol::PROTOCOL_VERSION;
pub use subc_transport::user_connection_token;
use subc_transport::{
    authenticate_client, connection_file, generate_daemon_id, generate_key, write_atomic,
    AuthError, ConnectionFileError, ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    net::{TcpListener, TcpStream},
    task::{JoinError, JoinHandle},
    time::{sleep, timeout},
};
use tracing::{error, info, warn};

use crate::{
    daemon_config::{self, ConfiguredModule, DaemonConfigError},
    server::{serve_listeners, ServerAuth, ServerError},
    supervise::HealthConfig,
    ConnectedClients, ControlHandler, DaemonSelfWatchdog, DaemonSelfWatchdogConfig,
    ForwardingTable, Registry, RestartPolicy, Router, Supervisor, SupervisorHandle,
    SupervisorProcessLiveness,
};
use std::sync::Arc;

pub const DEFAULT_SUBC_PORT: u16 = 8757;
pub const SUBC_PORT_ENV: &str = "SUBC_PORT";
use subc_transport::CONNECTION_FILE_NAME;
const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_AUTH_DEADLINE: Duration = Duration::from_secs(2);
const START_LOCK_RETRIES: usize = 40;
const START_LOCK_RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionFileSource {
    XdgRuntimeDir,
    TempDirFallback,
    Explicit,
}

impl ConnectionFileSource {
    fn reason(self) -> &'static str {
        match self {
            Self::XdgRuntimeDir => "XDG_RUNTIME_DIR set and non-empty",
            Self::TempDirFallback => "XDG_RUNTIME_DIR unset or empty",
            Self::Explicit => "configured path",
        }
    }
}

impl fmt::Display for ConnectionFileSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::XdgRuntimeDir => "xdg_runtime_dir",
            Self::TempDirFallback => "temp_dir_fallback",
            Self::Explicit => "explicit",
        })
    }
}

/// Runtime bootstrap configuration. Production uses the default fixed port and
/// optional daemon-config override; tests pass port 0 to let the OS assign a free
/// loopback port and discover it from the connection file.
#[derive(Debug, Clone, Default)]
struct AdmissionFactsConfig {
    carrier_module_id: Option<String>,
    targets: Option<Vec<String>>,
}

/// Controls where module cgroups are prepared.
///
/// In-process daemons default to [`Self::Disabled`] so they never derive a
/// production location from the host process. The shipped daemon explicitly uses
/// [`Self::Current`]. The `ck-subc` binary accepts
/// `SUBC_CGROUP_PLACEMENT=disabled` for isolated test processes; an unset
/// variable retains `Current`, and every other value is rejected at startup.
/// Tests that exercise placement can own a [`Self::Root`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CgroupPlacementConfig {
    #[default]
    Disabled,
    Current,
    Root(PathBuf),
}

#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    pub connection_file_path: PathBuf,
    pub port: u16,
    pub daemon_ver: String,
    configured_modules: Vec<ConfiguredModule>,
    storage_config: Option<daemon_config::StorageConfig>,
    admission_facts: AdmissionFactsConfig,
    daemon_config_path: Option<PathBuf>,
    configured_port: Option<u16>,
    /// Daemon-wide route.bind relay budget in milliseconds (the fallback for
    /// any module without a per-module override). `None` = built-in default
    /// (12s — see `control::DEFAULT_ROUTE_BIND_RELAY_TIMEOUT`).
    route_bind_relay_default_ms: Option<u64>,
    reserved_capabilities: BTreeMap<String, String>,
    watchdog_config: DaemonSelfWatchdogConfig,
    connection_file_source: ConnectionFileSource,
    /// Where module cgroups are prepared. Disabled unless a caller explicitly
    /// opts in, because `Current` derives a host location from `/proc/self/cgroup`.
    cgroup_placement: CgroupPlacementConfig,
    /// Directory the supervisor writes per-module stdout/stderr capture files
    /// into. `None` disables capture; the shipped binary supplies its real run
    /// directory explicitly.
    capture_logs_dir: Option<PathBuf>,
    /// File the supervisor appends every module's terminal exits to, so exit
    /// history survives a daemon restart. `None` keeps terminal history in
    /// memory only (each module's ring). Absent by default for the same reason
    /// as `capture_logs_dir`: an in-process daemon booted by a test must not
    /// append its exits to the operator's real `terminals.jsonl`, where they
    /// would show up in `ck module terminals`. The shipped binary supplies
    /// `<run dir>/terminals.jsonl` explicitly.
    terminal_journal_path: Option<PathBuf>,
    /// Where the machine id is read from, or minted into when absent. `None`
    /// serves no machine id. Absent by default for the same reason as
    /// `capture_logs_dir`: an in-process daemon booted by a test must never
    /// derive the operator's real data home and mint into it. The shipped binary
    /// supplies `<data home>/cortexkit/machine-id` explicitly.
    machine_id_path: Option<PathBuf>,
}

impl BootstrapConfig {
    pub fn new(connection_file_path: impl Into<PathBuf>, port: u16) -> Self {
        Self {
            connection_file_path: connection_file_path.into(),
            port,
            daemon_ver: DAEMON_VERSION.to_owned(),
            configured_modules: Vec::new(),
            storage_config: None,
            admission_facts: AdmissionFactsConfig::default(),
            daemon_config_path: None,
            configured_port: None,
            route_bind_relay_default_ms: None,
            reserved_capabilities: BTreeMap::new(),
            watchdog_config: DaemonSelfWatchdogConfig::default(),
            connection_file_source: ConnectionFileSource::Explicit,
            cgroup_placement: CgroupPlacementConfig::default(),
            capture_logs_dir: None,
            terminal_journal_path: None,
            machine_id_path: None,
        }
    }

    /// Serve the machine id stored at `path`, minting it there at startup when
    /// the file is absent. Embedding daemons and tests pass a path inside their
    /// own fixture tree.
    pub fn with_machine_id_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.machine_id_path = Some(path.into());
        self
    }

    /// Selects module cgroup placement. The default is disabled.
    pub fn with_cgroup_placement(mut self, placement: CgroupPlacementConfig) -> Self {
        self.cgroup_placement = placement;
        self
    }

    /// Redirects per-module stdout/stderr capture files out of the real run
    /// directory. Tests that start an in-process daemon must call this with a
    /// path inside their fixture tree.
    pub fn with_capture_logs_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.capture_logs_dir = Some(dir.into());
        self
    }

    /// Redirects the daemon-private journal, allowing embedded daemons and tests
    /// to keep their observations out of the operator's live run directory.
    pub fn with_terminal_journal_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.terminal_journal_path = Some(path.into());
        self
    }

    pub fn from_env() -> Result<Self, BootstrapError> {
        Self::from_env_with_daemon_config_path(daemon_config::default_config_path())
    }

    /// The SHIPPED BINARY's config, which is the only caller that should capture
    /// child output into the operator's real run directory.
    ///
    /// Kept separate from `from_env` deliberately: see `capture_logs_dir` and
    /// `terminal_journal_path` on this struct for why an absent value must mean
    /// NO CAPTURE and NO JOURNAL rather than the operator's real run directory.
    /// A run directory that cannot be resolved (a relative data home) refuses
    /// startup instead of landing under the working directory.
    pub fn from_env_for_daemon_binary() -> Result<Self, BootstrapError> {
        let run_dir = daemon_config::daemon_run_dir().map_err(BootstrapError::RunDir)?;
        let machine_id_path =
            crate::machine_id::default_machine_id_path().map_err(BootstrapError::MachineId)?;
        Ok(Self::from_env()?
            .with_capture_logs_dir(run_dir.join("logs"))
            .with_terminal_journal_path(run_dir.join("terminals.jsonl"))
            .with_machine_id_path(machine_id_path))
    }

    pub fn from_env_with_daemon_config_path(
        daemon_config_path: impl AsRef<Path>,
    ) -> Result<Self, BootstrapError> {
        let daemon_config_path = daemon_config_path.as_ref().to_path_buf();
        let daemon_config =
            daemon_config::load(&daemon_config_path).map_err(BootstrapError::DaemonConfig)?;
        let config_port = daemon_config.as_ref().and_then(|config| config.port);
        let storage_config = daemon_config
            .as_ref()
            .and_then(|config| config.storage.clone());
        let admission_facts_carrier_module_id = daemon_config
            .as_ref()
            .and_then(|config| config.admission_facts_carrier_module_id.clone());
        let admission_facts_targets = daemon_config
            .as_ref()
            .and_then(|config| config.admission_facts_targets.clone());
        let route_bind_relay_default_ms = daemon_config
            .as_ref()
            .and_then(|config| config.route_bind_relay_timeout_ms);
        let reserved_capabilities = daemon_config
            .as_ref()
            .map(|config| config.reserved_capabilities.clone())
            .unwrap_or_default();
        let configured_modules = daemon_config
            .map(|config| config.modules)
            .unwrap_or_default();

        let port = match env::var(SUBC_PORT_ENV) {
            Ok(raw) if !raw.trim().is_empty() => {
                let port = raw
                    .parse::<u16>()
                    .map_err(|source| BootstrapError::InvalidPort { raw, source })?;
                if let Some(config_port) = config_port {
                    info!(
                        env = SUBC_PORT_ENV,
                        env_port = port,
                        config_port,
                        "SUBC_PORT overrides daemon config port"
                    );
                }
                port
            }
            Ok(_) | Err(_) => config_port.unwrap_or(DEFAULT_SUBC_PORT),
        };

        let (connection_file_path, connection_file_source) =
            connection_file_path_with_source(non_empty_os_var("XDG_RUNTIME_DIR"));
        Ok(Self::new(connection_file_path, port)
            .with_configured_modules(configured_modules)
            .with_storage_config(storage_config)
            .with_admission_facts_config(admission_facts_carrier_module_id, admission_facts_targets)
            .with_route_bind_relay_default_ms(route_bind_relay_default_ms)
            .with_reserved_capabilities(reserved_capabilities)
            .with_daemon_config_source(daemon_config_path, config_port)
            .with_connection_file_source(connection_file_source))
    }

    pub fn with_daemon_config_path(
        self,
        daemon_config_path: impl AsRef<Path>,
    ) -> Result<Self, BootstrapError> {
        let daemon_config_path = daemon_config_path.as_ref().to_path_buf();
        let daemon_config =
            daemon_config::load(&daemon_config_path).map_err(BootstrapError::DaemonConfig)?;
        let configured_port = daemon_config.as_ref().and_then(|config| config.port);
        let storage_config = daemon_config
            .as_ref()
            .and_then(|config| config.storage.clone());
        let admission_facts_carrier_module_id = daemon_config
            .as_ref()
            .and_then(|config| config.admission_facts_carrier_module_id.clone());
        let admission_facts_targets = daemon_config
            .as_ref()
            .and_then(|config| config.admission_facts_targets.clone());
        let route_bind_relay_default_ms = daemon_config
            .as_ref()
            .and_then(|config| config.route_bind_relay_timeout_ms);
        let reserved_capabilities = daemon_config
            .as_ref()
            .map(|config| config.reserved_capabilities.clone())
            .unwrap_or_default();
        let configured_modules = daemon_config
            .map(|config| config.modules)
            .unwrap_or_default();
        Ok(self
            .with_configured_modules(configured_modules)
            .with_storage_config(storage_config)
            .with_admission_facts_config(admission_facts_carrier_module_id, admission_facts_targets)
            .with_route_bind_relay_default_ms(route_bind_relay_default_ms)
            .with_reserved_capabilities(reserved_capabilities)
            .with_daemon_config_source(daemon_config_path, configured_port))
    }

    pub fn with_configured_modules(
        mut self,
        modules: impl IntoIterator<Item = ConfiguredModule>,
    ) -> Self {
        self.configured_modules = modules.into_iter().collect();
        self.configured_modules
            .sort_by(|left, right| left.module_id.cmp(&right.module_id));
        self
    }

    pub fn with_storage_config(
        mut self,
        storage_config: Option<daemon_config::StorageConfig>,
    ) -> Self {
        self.storage_config = storage_config;
        self
    }

    pub fn with_admission_facts_config(
        mut self,
        carrier_module_id: Option<String>,
        targets: Option<Vec<String>>,
    ) -> Self {
        self.admission_facts = AdmissionFactsConfig {
            carrier_module_id,
            targets,
        };
        self
    }

    /// Set the daemon-wide route.bind relay default (the fallback for any
    /// module without a per-module override). `None` preserves the built-in
    /// default (12s). `serve_bound_daemon` reads this at startup and threads
    /// it into the control handler's daemon-wide field.
    pub fn with_route_bind_relay_default_ms(mut self, ms: Option<u64>) -> Self {
        self.route_bind_relay_default_ms = ms;
        self
    }

    pub fn with_reserved_capabilities(
        mut self,
        reserved_capabilities: BTreeMap<String, String>,
    ) -> Self {
        self.reserved_capabilities = reserved_capabilities;
        self
    }

    fn with_daemon_config_source(
        mut self,
        daemon_config_path: PathBuf,
        configured_port: Option<u16>,
    ) -> Self {
        self.daemon_config_path = Some(daemon_config_path);
        self.configured_port = configured_port;
        self
    }

    fn with_connection_file_source(mut self, source: ConnectionFileSource) -> Self {
        self.connection_file_source = source;
        self
    }

    pub fn with_watchdog_config(mut self, watchdog_config: DaemonSelfWatchdogConfig) -> Self {
        self.watchdog_config = watchdog_config;
        self
    }
}

/// Result of singleton discovery.
#[derive(Debug)]
pub enum Outcome {
    /// A live daemon authenticated from the connection file; this invocation should exit 0.
    AlreadyRunning,
    /// This process won the singleton race, owns bound loopback listener(s), and
    /// has published a fresh connection file.
    Bound(BoundDaemon),
}

#[derive(Debug)]
pub struct BoundDaemon {
    pub listeners: Vec<TcpListener>,
    pub connection_info: ConnectionInfo,
    pub connection_file_path: PathBuf,
    pub connection_file_source: ConnectionFileSource,
    /// The machine id this daemon serves, established before any connection is
    /// accepted. `None` when the config named no machine id path.
    pub machine_id: Option<crate::machine_id::MachineId>,
}

/// Resolve subc's per-user TCP connection-file path.
///
/// `$XDG_RUNTIME_DIR/subc-connection.json` is preferred because the runtime
/// directory is already per-user on Unix desktops. Without it, subc falls back
/// to the system temp dir with a per-user token in the filename so different OS
/// users do not collide on shared temp directories.
pub fn connection_file_path() -> PathBuf {
    connection_file_path_with_source(non_empty_os_var("XDG_RUNTIME_DIR")).0
}

fn connection_file_path_with_source(
    runtime_dir: Option<OsString>,
) -> (PathBuf, ConnectionFileSource) {
    if let Some(runtime_dir) = runtime_dir.filter(|value| !value.is_empty()) {
        return (
            PathBuf::from(runtime_dir).join(CONNECTION_FILE_NAME),
            ConnectionFileSource::XdgRuntimeDir,
        );
    }

    (
        env::temp_dir().join(format!("subc-{}.connection.json", user_connection_token())),
        ConnectionFileSource::TempDirFallback,
    )
}

/// Resolve, claim, and serve the per-user daemon singleton.
///
/// A second invocation is successful: if a live daemon authenticates from the
/// existing connection file, this returns `Ok(())` after logging and the caller
/// exits with status 0.
pub async fn run() -> Result<(), BootstrapError> {
    // `run` is a binary entry point, so it opts into the production cgroup and
    // child-log locations. In-process callers keep both features disabled by default.
    run_with_config(
        BootstrapConfig::from_env_for_daemon_binary()?
            .with_cgroup_placement(CgroupPlacementConfig::Current),
    )
    .await
}

/// Serve a daemon from an explicit config. This is the entry point the twelve
/// sibling repos use to boot an in-process daemon in their integration tests.
///
/// # THIS INSTALLS NO TRACING SUBSCRIBER, SO THE DAEMON IS SILENT BY DEFAULT
///
/// The daemon's own diagnostics go through `tracing`, and `tracing` DISCARDS
/// every event when no subscriber is installed. The shipped binary installs one
/// in `main` (`init_tracing`); this function deliberately does not, because a
/// library that installs a global subscriber fights with whatever the host
/// process already set up.
///
/// The consequence for a test harness is not "less verbose": it is that the
/// daemon has NOTHING TO SAY about any failure, and a missing instrument reads
/// exactly like a clean one. PLEX found this on 2026-09-18 while trying to
/// capture daemon logs beside an intermittent bind failure, and discovered the
/// daemon had been silent in every conformance run that repo had ever done --
/// so the one client-side error string was all the evidence that could exist,
/// and they had spent a real investigation on a failure whose second source was
/// never being recorded.
///
/// Install one in the harness before calling this, and assert it did something
/// (they measured 236 daemon lines with the subscriber installed, 0 with the
/// call commented out) -- otherwise the fix is itself unverified.
pub async fn run_with_config(config: BootstrapConfig) -> Result<(), BootstrapError> {
    let configured_modules = config.configured_modules.clone();
    let storage_config = config.storage_config.clone();
    let admission_facts = config.admission_facts.clone();
    let daemon_config_path = config.daemon_config_path.clone();
    let configured_port = config.configured_port;
    let route_bind_relay_default_ms = config.route_bind_relay_default_ms;
    let reserved_capabilities = config.reserved_capabilities.clone();
    let watchdog_config = config.watchdog_config.clone();
    let cgroup_placement_config = config.cgroup_placement.clone();
    let capture_logs_dir = config.capture_logs_dir.clone();
    let terminal_journal_path = config.terminal_journal_path.clone();
    match ensure_singleton_with_config(config).await? {
        Outcome::AlreadyRunning => {
            info!("subc daemon already running");
            Ok(())
        }
        Outcome::Bound(bound) => {
            #[cfg(target_os = "linux")]
            let cgroup_placement = prepare_cgroup_placement(&cgroup_placement_config);
            #[cfg(not(target_os = "linux"))]
            let _ = cgroup_placement_config;
            serve_bound_daemon(
                bound,
                configured_modules,
                storage_config,
                admission_facts,
                daemon_config_path,
                configured_port,
                route_bind_relay_default_ms,
                reserved_capabilities,
                watchdog_config,
                capture_logs_dir,
                terminal_journal_path,
                #[cfg(target_os = "linux")]
                cgroup_placement,
            )
            .await
        }
    }
}

#[cfg(target_os = "linux")]
fn prepare_cgroup_placement(config: &CgroupPlacementConfig) -> Option<subc_cgroup::Placement> {
    let result = match config {
        CgroupPlacementConfig::Disabled => return None,
        CgroupPlacementConfig::Current => subc_cgroup::prepare_current(),
        CgroupPlacementConfig::Root(root) => subc_cgroup::prepare_at(root),
    };

    match result {
        Ok(Some(placement)) => Some(placement),
        Ok(None) => {
            warn!(
                placement = ?config,
                "module cgroup placement is disabled: configured cgroup root is not delegated"
            );
            None
        }
        Err(error) => {
            warn!(
                placement = ?config,
                error = %error,
                "module cgroup placement is disabled by an unexpected cgroup probe error"
            );
            None
        }
    }
}

/// Target soft limit for open file descriptors, applied to the daemon before any
/// module is spawned so children inherit it. Multi-root modules (one process
/// aggregating every project root's sqlite stores, index caches, watchers, and
/// LSP pipes) trivially exceed the macOS default soft limit of 256; a launchd
/// user agent does not pass login-shell ulimits through, so the raise must
/// happen in-process.
#[cfg(unix)]
const NOFILE_TARGET: u64 = 65536;

/// Raise RLIMIT_NOFILE to `NOFILE_TARGET` (clamped to the hard limit).
/// Best-effort: failure is logged and never fatal, since the daemon can run
/// under the inherited limit — modules with few roots just have less headroom.
#[cfg(unix)]
fn raise_nofile_limit() {
    match rlimit::Resource::NOFILE.get() {
        Ok((soft, hard)) => {
            if soft >= NOFILE_TARGET {
                return;
            }
            let target = NOFILE_TARGET.min(hard);
            match rlimit::Resource::NOFILE.set(target, hard) {
                Ok(()) => info!(
                    previous_soft = soft,
                    new_soft = target,
                    hard,
                    "raised open-file soft limit for daemon and module children"
                ),
                Err(err) => warn!(
                    soft,
                    hard,
                    error = %err,
                    "could not raise open-file soft limit; multi-root modules may exhaust descriptors"
                ),
            }
        }
        Err(err) => warn!(error = %err, "could not read open-file limit"),
    }
}

/// CRT stdio-stream target on Windows (the `_setmaxstdio` maximum). Win32
/// HANDLEs — what Rust `File`, tokio sockets, and SQLite's Win32 VFS actually
/// consume — have a per-process quota in the millions and need no raise; the
/// C-runtime stream table (default 512) is the only low ceiling, and it is
/// per-process rather than inherited, so supervised modules linking the CRT
/// must raise their own. Raising it here covers the daemon itself.
#[cfg(windows)]
fn raise_nofile_limit() {
    const MAXSTDIO_TARGET: u32 = 8192;
    let current = rlimit::getmaxstdio();
    if current >= MAXSTDIO_TARGET {
        return;
    }
    match rlimit::setmaxstdio(MAXSTDIO_TARGET) {
        Ok(new_max) => info!(
            previous = current,
            new_max, "raised CRT stdio-stream limit for daemon"
        ),
        Err(err) => warn!(
            current,
            error = %err,
            "could not raise CRT stdio-stream limit"
        ),
    }
}

#[cfg(not(any(unix, windows)))]
fn raise_nofile_limit() {}

pub async fn run_with_daemon_config_path(
    config: BootstrapConfig,
    daemon_config_path: impl AsRef<Path>,
) -> Result<(), BootstrapError> {
    run_with_config(config.with_daemon_config_path(daemon_config_path)?).await
}

#[allow(clippy::too_many_arguments)]
async fn serve_bound_daemon(
    bound: BoundDaemon,
    configured_modules: Vec<ConfiguredModule>,
    storage_config: Option<daemon_config::StorageConfig>,
    admission_facts: AdmissionFactsConfig,
    daemon_config_path: Option<PathBuf>,
    configured_port: Option<u16>,
    route_bind_relay_default_ms: Option<u64>,
    reserved_capabilities: BTreeMap<String, String>,
    watchdog_config: DaemonSelfWatchdogConfig,
    capture_logs_dir: Option<PathBuf>,
    terminal_journal_path: Option<PathBuf>,
    #[cfg(target_os = "linux")] cgroup_placement: Option<subc_cgroup::Placement>,
) -> Result<(), BootstrapError> {
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(BootstrapError::Signal)?;
    // Windows has no SIGTERM. Ctrl-C is a different event, not an equivalent
    // service-stop contract, so this shutdown handler is intentionally Unix-only.
    raise_nofile_limit();

    info!(
        connection_file = %bound.connection_file_path.display(),
        connection_file_source = %bound.connection_file_source,
        connection_file_source_reason = bound.connection_file_source.reason(),
        endpoints = ?bound.connection_info.endpoints,
        configured_modules = configured_modules.len(),
        machine_id = bound.machine_id.as_ref().map(|id| id.as_str()).unwrap_or("none"),
        "subc daemon starting"
    );

    let registry = Arc::new(Registry::default());
    let process_liveness = Arc::new(SupervisorProcessLiveness::new());
    let supervisor_handle = SupervisorHandle::new();
    let connected_clients = ConnectedClients::new();
    let forwarding = Arc::new(ForwardingTable::default());
    let daemon_incarnation = format!(
        "{:032x}",
        u128::from_be_bytes(bound.connection_info.daemon_id)
    );
    let supervisor = Supervisor::new(Arc::clone(&registry), RestartPolicy::default())
        .with_process_liveness(process_liveness.clone())
        .with_forwarding(Arc::clone(&forwarding))
        .with_handle(supervisor_handle.clone())
        .with_connection_file_path(bound.connection_file_path.clone())
        .with_daemon_incarnation(daemon_incarnation.clone());
    // ABSENT MEANS NO CAPTURE AND NO JOURNAL, NOT "THE REAL RUN DIRECTORY", and
    // the difference is a production-corruption hazard rather than a preference.
    // Both fields below follow the same rule: `None` for `capture_logs_dir`
    // means supervised output is not captured, and `None` for
    // `terminal_journal_path` means terminal history lives only in each
    // module's in-memory ring (`supervisor.terminals` still answers, with the
    // journal counters at zero).
    //
    // The terminal journal used to fall back to
    // `daemon_run_dir().join("terminals.jsonl")`, so an in-process test daemon
    // appended its fixture exits to the operator's journal, where they then
    // appeared in `ck module terminals`.
    //
    // The capture line used to be `unwrap_or_else(|| daemon_run_dir().join("logs"))`,
    // so ANY caller that did not set the field captured supervised children into
    // the operator's live `~/.local/share/cortexkit/run/logs/`. That is twelve
    // sibling repos whose integration tests boot an in-process daemon through
    // `run_with_config` -- none of which asked for it, and none of which can see
    // it from their side.
    //
    // Harmless while fixture module ids are fixture-shaped: this host carries 20
    // zero-byte files from subc's own tests (good-aft, missing-aft,
    // preview-consumer...). THE HAZARD IS A COLLISION. A fixture named "broca"
    // or "aft" appends to a PRODUCTION capture file that operators read
    // forensically and that placement gates count lines in -- with no residue to
    // notice, because the file legitimately exists and legitimately grows.
    //
    // Found by BROCA (2026-09-19) from the other side: their rigs spawn the
    // SHIPPED ck-subc and set XDG_CONFIG_HOME + XDG_RUNTIME_DIR but not
    // XDG_DATA_HOME, so every local rig run supervised a module named "broca"
    // and captured it into production's broca.stderr.log -- the same file I
    // count seal lines in before and after placing their binaries.
    //
    // The supervisor already treats `None` as no-capture and no-journal, so
    // this only removes invented defaults. The binary keeps capturing and
    // journaling via `BootstrapConfig::from_env_for_daemon_binary`.
    let supervisor = match terminal_journal_path {
        Some(path) => supervisor.with_terminal_journal(path, daemon_incarnation),
        None => supervisor,
    };
    let supervisor = match capture_logs_dir {
        Some(dir) => supervisor.with_capture_logs_dir(dir),
        None => supervisor,
    };
    #[cfg(target_os = "linux")]
    let supervisor = supervisor.with_cgroup_placement(cgroup_placement);
    // Collect per-module route.bind relay overrides BEFORE handing the
    // `configured_modules` vector to the supervisor (which only needs each
    // module's `drain_timeout_ms`). Each entry was filled in by parse-time
    // resolution (per-module > daemon-wide > absent), so modules with no
    // override are absent from this map and the daemon-wide default applies.
    let route_bind_relay_timeouts = configured_modules
        .iter()
        .filter_map(|module| {
            module
                .route_bind_relay_timeout_ms
                .map(|ms| (module.module_id.clone(), Duration::from_millis(ms)))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let control_start_clock = crate::clock::StartClock::capture();
    let mut control = ControlHandler::with_forwarding(Arc::clone(&registry), forwarding)
        .with_process_liveness(process_liveness)
        .with_supervisor(supervisor_handle)
        .with_connected_clients(connected_clients.clone())
        .with_storage_config(storage_config)
        .with_machine_id(bound.machine_id.clone())
        .with_admission_facts_config(admission_facts.carrier_module_id, admission_facts.targets)
        .with_route_bind_relay_timeouts(route_bind_relay_timeouts)
        .with_daemon_provenance(
            bound.connection_info.pid,
            control_start_clock.started_at_ms(),
            std::env::current_exe().ok(),
            normalized_build_provenance(env!("SUBC_BUILD_GIT_SHA")),
            normalized_build_provenance(env!("SUBC_BUILD_LOCK_DIGEST")),
        )
        .with_daemon_start_clock(control_start_clock)
        .with_capability_config(
            configured_modules
                .iter()
                .map(|module| (module.module_id.clone(), module.enabled)),
            reserved_capabilities,
        );
    if let Some(ms) = route_bind_relay_default_ms {
        // A daemon-wide config value overrides the built-in default; a
        // `None` here leaves the ControlHandler's 12s default in place.
        control = control.with_route_bind_relay_timeout(Duration::from_millis(ms));
    }
    if let Some(config_path) = daemon_config_path {
        control = control.with_supervisor_rescan(supervisor.clone(), config_path, configured_port);
    }
    let control = Arc::new(control);
    let router = Arc::new(Router::with_control_handler(Arc::clone(&control)));
    let auth = ServerAuth::new(
        bound.connection_info.key.clone(),
        bound.connection_info.daemon_id,
        bound.connection_info.daemon_ver.clone(),
    )
    .with_connected_clients(connected_clients);

    let mut serve_task =
        AbortOnDrop::new(tokio::spawn(serve_listeners(bound.listeners, router, auth)));
    tokio::task::yield_now().await;
    let _clock_step_task = AbortOnDrop::new(crate::watchdog::spawn_clock_step_monitor());
    // Off the startup path: it runs `systemctl`, and only ever logs.
    #[cfg(target_os = "linux")]
    let _kill_mode_check = AbortOnDrop::new(tokio::spawn(
        crate::systemd_kill_mode::warn_if_kill_mode_defeats_ordered_shutdown(),
    ));
    let _watchdog_task = AbortOnDrop::new(
        DaemonSelfWatchdog::new(
            bound.connection_info.clone(),
            bound.connection_file_path.clone(),
        )
        .with_config(watchdog_config)
        .spawn(),
    );

    for configured in configured_modules {
        let enabled = configured.enabled;
        let health = configured.health;
        let module_id = configured.module_id.clone();
        match supervisor.supervise_configured_with_health(
            configured.module_spec(),
            enabled,
            health,
            configured.drain_timeout_ms,
            configured.restart,
        ) {
            Ok(_) => {
                // A raised failure threshold is normally a temporary allowance for a
                // drive that deliberately stops a module, and it widens the window in
                // which a genuinely wedged module looks fine. It is only ever noticed
                // when someone thinks to re-read the config, so a relaxation outlives
                // its reason silently: a rig ran five days at 240s of tolerance against
                // a 90s default because a comment promising a revert was mistaken for
                // the revert. Saying so on every boot costs one line and removes the
                // need for anyone to remember.
                let default_threshold = HealthConfig::default().failure_threshold;
                if enabled && health.failure_threshold > default_threshold {
                    warn!(
                        module_id = %module_id,
                        failure_threshold = health.failure_threshold,
                        default_threshold,
                        tolerance_secs = health.cadence.as_secs() * u64::from(health.failure_threshold),
                        "health failure threshold is relaxed above the default; a wedged module stays unflagged for longer"
                    );
                }
                info!(module_id = %module_id, enabled, "configured module supervised");
            }
            Err(err) => {
                error!(module_id = %module_id, error = %err, "failed to supervise configured module; continuing daemon startup");
            }
        }
    }

    control.refresh_capability_requirements();
    Arc::clone(&control).spawn_capability_deadline_loop();

    #[cfg(unix)]
    {
        tokio::select! {
            result = serve_task.join() => {
                return result.map_err(BootstrapError::ServeJoin)?.map_err(BootstrapError::Serve);
            }
            _ = terminate.recv() => {}
        }
        // First, before the notice, the drain, or any connection close: from
        // here on a module exit is recorded as `daemon_shutdown` and never
        // respawned. Also before allowing a second signal to cut the bounded
        // wait short, so the journal marker is always written.
        supervisor.begin_daemon_shutdown();
        // Dropping the listener stops new accepts, not established connections:
        // their detached tasks must remain live throughout notice and drain.
        drop(serve_task);
        let escalated = tokio::select! {
            biased;
            _ = terminate.recv() => {
                info!("second SIGTERM: abandoning daemon shutdown wait");
                true
            }
            result = supervisor.drain_for_daemon_shutdown() => {
                if let Err(error) = result {
                    warn!(%error, "daemon shutdown drain failed; exiting anyway");
                }
                false
            }
        };
        // Supervised modules lead their own process groups, so a service
        // manager's kill of this process's group does not reach them. (A
        // systemd unit with KillMode=control-group kills by cgroup instead and
        // does reach them; see `systemd_kill_mode`.) The daemon ends them
        // itself: EOF (or SIGTERM for a protocol none child) first, then
        // signals at each child's own drain deadline.
        supervisor
            .end_children_for_daemon_shutdown(escalated, async {
                terminate.recv().await;
            })
            .await;
        Ok(())
    }
    #[cfg(not(unix))]
    serve_task
        .join()
        .await
        .map_err(BootstrapError::ServeJoin)?
        .map_err(BootstrapError::Serve)
}

fn normalized_build_provenance(value: &str) -> Option<String> {
    match value.trim() {
        "" | "unavailable" => None,
        value => Some(value.to_string()),
    }
}

/// Find an existing daemon or atomically bind loopback TCP for this daemon.
///
/// The algorithm is intentionally connect-first: an endpoint from the connection
/// file is treated as live only after the TCP+key server-proof authenticates for
/// that file's key and daemon_id. Stale or foreign connection files are reclaimed
/// only while holding the per-user start lock; the TCP port is never the
/// singleton primitive.
pub async fn ensure_singleton(
    connection_file_path: impl AsRef<Path>,
    port: u16,
) -> Result<Outcome, BootstrapError> {
    ensure_singleton_with_config(BootstrapConfig::new(connection_file_path.as_ref(), port)).await
}

pub async fn ensure_singleton_with_config(
    config: BootstrapConfig,
) -> Result<Outcome, BootstrapError> {
    let path = config.connection_file_path;

    if matches!(probe_existing(&path).await?, Probe::Live) {
        return Ok(Outcome::AlreadyRunning);
    }

    let _lock = StartLock::acquire(&path).await?;

    // Re-probe after acquiring the start lock so a peer that won the race between
    // our first failed probe and the lock acquisition is observed instead of
    // overwritten.
    if matches!(probe_existing(&path).await?, Probe::Live) {
        return Ok(Outcome::AlreadyRunning);
    }

    remove_stale_connection_file_if_present(&path)?;

    // Established under the start lock and before binding, so a corrupt file
    // stops boot before anything is published, and two daemons racing to start
    // cannot both mint.
    let machine_id = config
        .machine_id_path
        .as_deref()
        .map(crate::machine_id::load_or_mint)
        .transpose()
        .map_err(BootstrapError::MachineId)?;

    let (listeners, endpoints) = bind_loopback(config.port).await?;
    let connection_info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: Some(PROTOCOL_VERSION),
        endpoints,
        key: generate_key().map_err(BootstrapError::GenerateConnectionFile)?,
        daemon_id: generate_daemon_id().map_err(BootstrapError::GenerateConnectionFile)?,
        pid: process::id(),
        daemon_ver: config.daemon_ver,
    };

    if let Err(source) = write_atomic(&path, &connection_info) {
        drop(listeners);
        return Err(BootstrapError::ConnectionFileWrite { path, source });
    }

    Ok(Outcome::Bound(BoundDaemon {
        listeners,
        connection_info,
        connection_file_path: path,
        connection_file_source: config.connection_file_source,
        machine_id,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Live,
    StaleOrAbsent,
}

async fn probe_existing(path: &Path) -> Result<Probe, BootstrapError> {
    let info = match connection_file::read(path) {
        Ok(info) => info,
        Err(source) if is_absent_or_stale_connection_file(&source) => {
            return Ok(Probe::StaleOrAbsent)
        }
        Err(source) => {
            return Err(BootstrapError::ConnectionFileRead {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    for endpoint in &info.endpoints {
        if matches!(probe_endpoint(&info, endpoint).await, Probe::Live) {
            return Ok(Probe::Live);
        }
    }

    Ok(Probe::StaleOrAbsent)
}

async fn probe_endpoint(info: &ConnectionInfo, endpoint: &Endpoint) -> Probe {
    let Ok(ip) = endpoint.host.parse::<IpAddr>() else {
        return Probe::StaleOrAbsent;
    };
    if !ip.is_loopback() {
        return Probe::StaleOrAbsent;
    }
    let addr = SocketAddr::new(ip, endpoint.port);

    let mut stream = match timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) | Err(_) => return Probe::StaleOrAbsent,
    };

    match authenticate_client(&mut stream, info, PROBE_AUTH_DEADLINE).await {
        Ok(()) => Probe::Live,
        Err(AuthError::DaemonIdMismatch)
        | Err(AuthError::InvalidServerProof)
        | Err(AuthError::UnexpectedEof { .. })
        | Err(AuthError::Timeout { .. })
        | Err(AuthError::JsonEncode { .. })
        | Err(AuthError::JsonDecode { .. })
        | Err(AuthError::Io { .. })
        | Err(AuthError::MessageTooLarge { .. })
        | Err(AuthError::KeyTooShort { .. })
        | Err(AuthError::Random(_))
        | Err(AuthError::InvalidClientAuth) => Probe::StaleOrAbsent,
    }
}

fn is_absent_or_stale_connection_file(err: &ConnectionFileError) -> bool {
    match err {
        ConnectionFileError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => true,
        ConnectionFileError::JsonRead { .. }
        | ConnectionFileError::UnsupportedSchema { .. }
        | ConnectionFileError::Invalid { .. }
        | ConnectionFileError::KeyTooShort { .. }
        // A live daemon always publishes the file owner-only (0600), so a file
        // with insecure permissions is never a daemon we should defer to: treat it
        // as stale and take over (which republishes a correct 0600 file).
        | ConnectionFileError::InsecurePermissions { .. } => true,
        ConnectionFileError::MissingParent { .. }
        | ConnectionFileError::MissingFileName { .. }
        // A writable ancestor is an operator misconfiguration, never evidence
        // about whether a daemon is live. Reclaiming the file would republish key
        // material into the same directory the refusal is about.
        | ConnectionFileError::InsecureParentDirectory { .. }
        | ConnectionFileError::Io { .. }
        | ConnectionFileError::JsonWrite { .. }
        | ConnectionFileError::Random(_)
        // A wire mismatch may identify a newer live daemon, so never reclaim its
        // connection file merely because this binary cannot speak its envelope.
        | ConnectionFileError::WireVersionMismatch { .. } => false,
    }
}

fn remove_stale_connection_file_if_present(path: &Path) -> Result<(), BootstrapError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BootstrapError::RemoveStale {
            path: path.to_path_buf(),
            source,
        }),
    }
}

async fn bind_loopback(port: u16) -> Result<(Vec<TcpListener>, Vec<Endpoint>), BootstrapError> {
    let v4_host = Ipv4Addr::LOCALHOST;
    let v4 = TcpListener::bind((v4_host, port))
        .await
        .map_err(|source| BootstrapError::Bind {
            host: v4_host.to_string(),
            port,
            source,
        })?;
    let actual_port = v4
        .local_addr()
        .map_err(|source| BootstrapError::LocalAddr {
            host: v4_host.to_string(),
            source,
        })?
        .port();

    let mut listeners = vec![v4];
    let mut endpoints = vec![Endpoint {
        host: v4_host.to_string(),
        port: actual_port,
    }];

    let v6_host = Ipv6Addr::LOCALHOST;
    match TcpListener::bind((v6_host, actual_port)).await {
        Ok(v6) => {
            listeners.push(v6);
            endpoints.push(Endpoint {
                host: v6_host.to_string(),
                port: actual_port,
            });
        }
        Err(err) if ipv6_loopback_unavailable(&err) => {
            warn!(
                port = actual_port,
                error = %err,
                "IPv6 loopback unavailable; serving only IPv4 loopback"
            );
        }
        Err(source) => {
            drop(listeners);
            return Err(BootstrapError::Bind {
                host: v6_host.to_string(),
                port: actual_port,
                source,
            });
        }
    }

    Ok((listeners, endpoints))
}

fn ipv6_loopback_unavailable(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
    ) || matches!(err.raw_os_error(), Some(47) | Some(49) | Some(97))
}

struct AbortOnDrop<T> {
    handle: JoinHandle<T>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self { handle }
    }

    async fn join(&mut self) -> Result<T, JoinError> {
        (&mut self.handle).await
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if !self.handle.is_finished() {
            self.handle.abort();
        }
    }
}

struct StartLock {
    // Keep the locked file handle alive for the duration of bootstrap; closing
    // it releases the advisory lock while leaving the stable path in place.
    _file: fs::File,
}

impl StartLock {
    async fn acquire(connection_file_path: &Path) -> Result<Self, BootstrapError> {
        let path = start_lock_path(connection_file_path);
        for _ in 0..START_LOCK_RETRIES {
            let file = match open_owner_only_lock(&path) {
                Ok(file) => file,
                Err(source) => return Err(BootstrapError::StartLockCreate { path, source }),
            };
            match FileExt::try_lock(&file) {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) => sleep(START_LOCK_RETRY_DELAY).await,
                Err(TryLockError::Error(source)) => {
                    return Err(BootstrapError::StartLockCreate { path, source });
                }
            }
        }

        Err(BootstrapError::StartLockBusy {
            path,
            attempts: START_LOCK_RETRIES,
        })
    }
}

fn open_owner_only_lock(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn start_lock_path(connection_file_path: &Path) -> PathBuf {
    let file_name = connection_file_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| CONNECTION_FILE_NAME.into());
    let lock_name = format!("{file_name}.start-lock");
    connection_file_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(lock_name)
}

fn non_empty_os_var(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Bootstrap-layer errors are deliberately typed so startup never panics for
/// ordinary daemon-discovery races or stale filesystem state.
#[derive(Debug)]
pub enum BootstrapError {
    #[cfg(unix)]
    Signal(io::Error),
    InvalidPort {
        raw: String,
        source: std::num::ParseIntError,
    },
    ConnectionFileRead {
        path: PathBuf,
        source: ConnectionFileError,
    },
    ConnectionFileWrite {
        path: PathBuf,
        source: ConnectionFileError,
    },
    GenerateConnectionFile(ConnectionFileError),
    StartLockCreate {
        path: PathBuf,
        source: io::Error,
    },
    StartLockBusy {
        path: PathBuf,
        attempts: usize,
    },
    RemoveStale {
        path: PathBuf,
        source: io::Error,
    },
    Bind {
        host: String,
        port: u16,
        source: io::Error,
    },
    LocalAddr {
        host: String,
        source: io::Error,
    },
    DaemonConfig(DaemonConfigError),
    /// The machine id could not be established: its file is corrupt, unreadable
    /// or unwritable, or the data home is relative. The daemon does not start.
    MachineId(crate::machine_id::MachineIdFileError),
    /// The daemon run directory could not be resolved because the data home is
    /// relative. The daemon does not start.
    RunDir(daemon_config::DaemonRunDirError),
    Serve(ServerError),
    ServeJoin(tokio::task::JoinError),
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(unix)]
            Self::Signal(error) => write!(f, "failed to register SIGTERM handler: {error}"),
            Self::InvalidPort { raw, source } => {
                write!(f, "invalid {SUBC_PORT_ENV} value '{raw}': {source}")
            }
            Self::ConnectionFileRead { path, source } => write!(
                f,
                "failed to read connection file {}: {source}",
                path.display()
            ),
            Self::ConnectionFileWrite { path, source } => write!(
                f,
                "failed to publish connection file {}: {source}",
                path.display()
            ),
            Self::GenerateConnectionFile(err) => {
                write!(f, "failed to generate connection-file auth material: {err}")
            }
            Self::StartLockCreate { path, source } => {
                write!(
                    f,
                    "failed to create start lock {}: {source}",
                    path.display()
                )
            }
            Self::StartLockBusy { path, attempts } => write!(
                f,
                "start lock {} remained busy after {attempts} attempts",
                path.display()
            ),
            Self::RemoveStale { path, source } => write!(
                f,
                "failed to remove stale connection file {}: {source}",
                path.display()
            ),
            Self::Bind { host, port, source } if source.kind() == io::ErrorKind::AddrInUse => {
                write!(
                    f,
                    "port {port} in use on loopback {host}: {source}; set the port in config"
                )
            }
            Self::Bind { host, port, source } => {
                write!(f, "failed to bind loopback TCP {host}:{port}: {source}")
            }
            Self::LocalAddr { host, source } => {
                write!(f, "failed to read local address for {host}: {source}")
            }
            Self::DaemonConfig(err) => write!(f, "failed to load daemon config: {err}"),
            Self::MachineId(err) => write!(f, "refusing to start: {err}"),
            Self::RunDir(err) => write!(f, "refusing to start: {err}"),
            Self::Serve(err) => write!(f, "daemon server failed: {err}"),
            Self::ServeJoin(err) => write!(f, "daemon server task failed: {err}"),
        }
    }
}

impl Error for BootstrapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            #[cfg(unix)]
            Self::Signal(source) => Some(source),
            Self::InvalidPort { source, .. } => Some(source),
            Self::ConnectionFileRead { source, .. }
            | Self::ConnectionFileWrite { source, .. }
            | Self::GenerateConnectionFile(source) => Some(source),
            Self::StartLockCreate { source, .. }
            | Self::RemoveStale { source, .. }
            | Self::Bind { source, .. }
            | Self::LocalAddr { source, .. } => Some(source),
            Self::DaemonConfig(err) => Some(err),
            Self::MachineId(err) => Some(err),
            Self::RunDir(err) => Some(err),
            Self::Serve(err) => Some(err),
            Self::ServeJoin(err) => Some(err),
            Self::StartLockBusy { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ServerAuth;
    use crate::test_support::TestTempDir;
    #[cfg(target_os = "linux")]
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    #[cfg(target_os = "linux")]
    use subc_control::ModuleProtocol;
    use subc_transport::MIN_KEY_LEN;
    use tokio::io::AsyncReadExt;
    use tokio::task::JoinHandle;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn normalized_build_provenance_preserves_real_values() {
        assert_eq!(normalized_build_provenance("abc"), Some("abc".to_string()));
    }

    #[test]
    fn normalized_build_provenance_omits_unavailable_and_empty_values() {
        assert_eq!(normalized_build_provenance("unavailable"), None);
        assert_eq!(normalized_build_provenance(""), None);
    }

    #[cfg(target_os = "linux")]
    fn current_cgroup_path_for_test() -> io::Result<PathBuf> {
        let cgroups = fs::read_to_string("/proc/self/cgroup")?;
        let relative = cgroups
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "cgroup v2 is unavailable")
            })?;
        Ok(Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')))
    }

    #[cfg(target_os = "linux")]
    fn module_cgroup_directories() -> io::Result<BTreeSet<OsString>> {
        let modules = current_cgroup_path_for_test()?.join("subc-modules");
        let entries = match fs::read_dir(modules) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(error) => return Err(error),
        };
        let mut directories = BTreeSet::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories.insert(entry.file_name());
            }
        }
        Ok(directories)
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_path(path: &Path, task: &JoinHandle<Result<(), BootstrapError>>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !path.exists() && tokio::time::Instant::now() < deadline {
            assert!(
                !task.is_finished(),
                "daemon exited before creating {}",
                path.display()
            );
            sleep(Duration::from_millis(10)).await;
        }
        assert!(path.exists(), "daemon did not create {}", path.display());
    }

    /// An in-process daemon with the default (disabled) cgroup placement must
    /// leave the host's live module cgroup tree exactly as it found it.
    /// Red-checking this test (making it fail) creates a
    /// `cgroup-isolation-probe-*` directory in the live cgroup tree, which must be removed by hand.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_with_config_does_not_reconcile_the_ambient_cgroup_by_default() {
        let temp = unique_temp_dir("bootstrap-cgroup-default-disabled");
        let module_id = format!("cgroup-isolation-probe-{}", process::id());
        let before = module_cgroup_directories().expect("read ambient module cgroups before boot");
        assert!(
            !before.contains(&OsString::from(&module_id)),
            "isolation probe cgroup already exists before this daemon starts"
        );
        let capture = temp.join("logs").join(format!("{module_id}.stderr.log"));
        let module = ConfiguredModule {
            module_id,
            program: PathBuf::from("sh"),
            args: vec!["-c".to_string(), "sleep 30".to_string()],
            env: Vec::new(),
            log: None,
            enabled: true,
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
            health: HealthConfig::default(),
            drain_timeout_ms: None,
            route_bind_relay_timeout_ms: None,
            restart: RestartPolicy::default(),
        };
        let config = BootstrapConfig::new(temp.join("connection.json"), 0)
            .with_configured_modules([module])
            .with_capture_logs_dir(temp.join("logs"))
            .with_terminal_journal_path(temp.join("terminals.jsonl"));
        let task = tokio::spawn(run_with_config(config));

        wait_for_path(&capture, &task).await;
        let after = module_cgroup_directories().expect("read ambient module cgroups after boot");

        task.abort();
        assert!(task
            .await
            .expect_err("aborted daemon task must cancel")
            .is_cancelled());
        assert_eq!(
            before, after,
            "an in-process daemon must not create or reconcile ambient module cgroups"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_cgroup_root_is_prepared_inside_the_fixture_tree() {
        let temp = unique_temp_dir("bootstrap-cgroup-explicit-root");
        let cgroup_root = temp.join("cgroup");
        fs::create_dir(&cgroup_root).expect("create scratch cgroup root");
        fs::write(cgroup_root.join("cgroup.procs"), b"").expect("write scratch cgroup marker");
        let modules = cgroup_root.join("subc-modules");
        let config = BootstrapConfig::new(temp.join("connection.json"), 0)
            .with_cgroup_placement(CgroupPlacementConfig::Root(cgroup_root))
            .with_terminal_journal_path(temp.join("terminals.jsonl"));
        let task = tokio::spawn(run_with_config(config));

        wait_for_path(&modules, &task).await;

        task.abort();
        assert!(task
            .await
            .expect_err("aborted daemon task must cancel")
            .is_cancelled());
        assert!(
            fs::read_dir(&modules)
                .expect("read prepared modules directory")
                .next()
                .is_none(),
            "the delegation probe must clean up after itself"
        );
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = env::var_os(key);
            env::set_var(key, value);
            Self { key, previous }
        }

        fn set_str(key: &'static str, value: &str) -> Self {
            let previous = env::var_os(key);
            env::set_var(key, value);
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = env::var_os(key);
            env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }

    fn unique_temp_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    fn temp_connection_file_path(name: &str) -> (TestTempDir, PathBuf) {
        let dir = unique_temp_dir(name);
        let path = dir.join("conn.json");
        (dir, path)
    }

    fn auth_for(info: &ConnectionInfo) -> ServerAuth {
        ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone())
    }

    fn start_server(bound: BoundDaemon) -> JoinHandle<Result<(), ServerError>> {
        let auth = auth_for(&bound.connection_info);
        tokio::spawn(serve_listeners(
            bound.listeners,
            Arc::new(Router::with_default_self_handler()),
            auth,
        ))
    }

    fn expect_bound(outcome: Outcome) -> BoundDaemon {
        match outcome {
            Outcome::Bound(bound) => bound,
            Outcome::AlreadyRunning => panic!("fresh connection file unexpectedly had a daemon"),
        }
    }

    async fn connect_from_info(conn: &ConnectionInfo) -> io::Result<TcpStream> {
        let endpoint = conn
            .endpoints
            .first()
            .expect("test connection file should have an endpoint");
        let ip: IpAddr = endpoint.host.parse().unwrap();
        TcpStream::connect(SocketAddr::new(ip, endpoint.port)).await
    }

    fn make_connection_info(port: u16) -> ConnectionInfo {
        ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_owned(),
                port,
            }],
            key: generate_key().unwrap(),
            daemon_id: generate_daemon_id().unwrap(),
            pid: process::id(),
            daemon_ver: "test-subc".to_owned(),
        }
    }

    fn write_raw_owner_only_connection_file(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn assert_owner_only_connection_file(path: &Path) {
        // `path` is only inspected on Unix (mode bits); on Windows the owner-only
        // guarantee comes from the inherited %TEMP% ACL, nothing to assert here.
        #[cfg(unix)]
        {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    #[test]
    fn connection_file_path_uses_xdg_runtime_dir_when_set() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let runtime_dir = unique_temp_dir("xdg-runtime");
        let _xdg = EnvGuard::set("XDG_RUNTIME_DIR", runtime_dir.path());

        assert_eq!(
            connection_file_path(),
            runtime_dir.join(CONNECTION_FILE_NAME)
        );
    }

    #[test]
    fn connection_file_path_source_is_xdg_runtime_dir_when_set() {
        let runtime_dir = OsString::from("/run/user/1000");

        let (path, source) = connection_file_path_with_source(Some(runtime_dir));

        assert_eq!(
            path,
            PathBuf::from("/run/user/1000").join(CONNECTION_FILE_NAME)
        );
        assert_eq!(source, ConnectionFileSource::XdgRuntimeDir);
    }

    #[test]
    fn connection_file_path_falls_back_to_temp_dir_with_user_token_when_xdg_unset() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let _xdg = EnvGuard::unset("XDG_RUNTIME_DIR");

        assert_eq!(
            connection_file_path(),
            env::temp_dir().join(format!("subc-{}.connection.json", user_connection_token()))
        );
    }

    #[test]
    fn connection_file_path_source_is_temp_dir_when_xdg_unset() {
        let (path, source) = connection_file_path_with_source(None);

        assert_eq!(
            path,
            env::temp_dir().join(format!("subc-{}.connection.json", user_connection_token()))
        );
        assert_eq!(source, ConnectionFileSource::TempDirFallback);
    }

    /// Concurrent callers must derive one token. The former temp-file uid probe
    /// could fail transiently (same-tick name collision, fd exhaustion) and send
    /// the loser down the env-derived fallback with a different identity for the
    /// same user; this fence keeps identity independent of filesystem luck.
    #[test]
    fn user_connection_token_is_stable_under_concurrent_callers() {
        let expected = user_connection_token();
        let workers: Vec<_> = (0..32)
            .map(|_| {
                std::thread::spawn(|| (0..40).map(|_| user_connection_token()).collect::<Vec<_>>())
            })
            .collect();
        for worker in workers {
            for token in worker.join().expect("probe thread") {
                assert_eq!(token, expected, "token diverged under concurrent probes");
            }
        }
    }

    #[test]
    fn connection_file_path_source_is_temp_dir_when_xdg_empty() {
        let (path, source) = connection_file_path_with_source(Some(OsString::new()));

        assert_eq!(
            path,
            env::temp_dir().join(format!("subc-{}.connection.json", user_connection_token()))
        );
        assert_eq!(source, ConnectionFileSource::TempDirFallback);
    }

    /// The shipped binary is the only caller that journals terminal exits into
    /// the real run directory, so its constructor must supply that path itself;
    /// the in-process default leaves it unset.
    #[test]
    fn daemon_binary_config_journals_and_captures_into_the_run_dir() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let root = unique_temp_dir("daemon-binary-config");
        let data_home = root.join("data");
        let _data = EnvGuard::set("XDG_DATA_HOME", &data_home);
        let _config = EnvGuard::set("XDG_CONFIG_HOME", &root.join("config"));
        let _port = EnvGuard::unset(SUBC_PORT_ENV);

        let config = BootstrapConfig::from_env_for_daemon_binary().unwrap();

        let run_dir = data_home.join("cortexkit").join("run");
        assert_eq!(
            config.terminal_journal_path,
            Some(run_dir.join("terminals.jsonl"))
        );
        assert_eq!(config.capture_logs_dir, Some(run_dir.join("logs")));
        assert_eq!(
            BootstrapConfig::new(root.join("connection.json"), 0).terminal_journal_path,
            None,
            "an in-process config must not journal anywhere unless asked to"
        );
    }

    #[test]
    fn daemon_binary_config_refuses_a_relative_data_home() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let root = unique_temp_dir("daemon-binary-relative-data");
        let _data = EnvGuard::set_str("XDG_DATA_HOME", "relative-data-home");
        let _config = EnvGuard::set("XDG_CONFIG_HOME", &root.join("config"));
        let _port = EnvGuard::unset(SUBC_PORT_ENV);

        let error = BootstrapConfig::from_env_for_daemon_binary()
            .expect_err("a relative data home must refuse the daemon binary's config");
        assert!(
            matches!(error, BootstrapError::RunDir(_)),
            "expected a run-directory refusal, got {error}"
        );
        assert!(error.to_string().contains("XDG_DATA_HOME"), "{error}");
    }

    #[test]
    fn configured_port_uses_default_config_and_env_override() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let (_dir, conn_path) = temp_connection_file_path("daemon-config-port");
        let config_path = conn_path.with_file_name("subc.jsonc");

        let _port = EnvGuard::unset(SUBC_PORT_ENV);
        assert_eq!(
            BootstrapConfig::from_env_with_daemon_config_path(&config_path)
                .unwrap()
                .port,
            DEFAULT_SUBC_PORT
        );

        fs::write(&config_path, r#"{ "version": 1, "port": 8123 }"#).unwrap();
        assert_eq!(
            BootstrapConfig::from_env_with_daemon_config_path(&config_path)
                .unwrap()
                .port,
            8123
        );

        let _port = EnvGuard::set_str(SUBC_PORT_ENV, "9012");
        assert_eq!(
            BootstrapConfig::from_env_with_daemon_config_path(&config_path)
                .unwrap()
                .port,
            9012
        );
    }

    #[tokio::test]
    async fn second_singleton_probe_against_served_tcp_daemon_reports_already_running() {
        let (_dir, path) = temp_connection_file_path("already-running");

        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        let server = start_server(bound);

        let second = ensure_singleton(&path, 0).await.unwrap();
        assert!(matches!(second, Outcome::AlreadyRunning));

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn daemon_connection_file_publishes_protocol_wire_version() {
        let (_dir, path) = temp_connection_file_path("wire-version");
        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        assert_eq!(bound.connection_info.wire_version, Some(PROTOCOL_VERSION));
        assert_eq!(
            connection_file::read(&path).unwrap().wire_version,
            Some(PROTOCOL_VERSION)
        );

        drop(bound.listeners);
    }

    #[tokio::test]
    async fn stale_unbound_connection_file_is_reclaimed() {
        let (_dir, path) = temp_connection_file_path("stale-reclaim");
        let stale = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let stale_port = stale.local_addr().unwrap().port();
        drop(stale);
        let stale_info = make_connection_info(stale_port);
        write_atomic(&path, &stale_info).unwrap();

        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        assert_ne!(bound.connection_info.key, stale_info.key);
        drop(bound.listeners);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_singleton_reclaims_insecure_connection_file() {
        let (_dir, path) = temp_connection_file_path("insecure-reclaim");
        let stale = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let stale_port = stale.local_addr().unwrap().port();
        drop(stale);
        let stale_info = make_connection_info(stale_port);
        write_atomic(&path, &stale_info).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        assert_ne!(bound.connection_info.key, stale_info.key);
        assert_ne!(bound.connection_info.daemon_id, stale_info.daemon_id);
        assert_owner_only_connection_file(&path);

        drop(bound.listeners);
    }

    #[tokio::test]
    async fn ensure_singleton_reclaims_non_loopback_connection_file() {
        let (_dir, path) = temp_connection_file_path("non-loopback-reclaim");
        let mut stale_info = make_connection_info(8757);
        stale_info.endpoints = vec![Endpoint {
            host: "192.0.2.10".to_owned(),
            port: 8757,
        }];
        write_atomic(&path, &stale_info).unwrap();

        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        assert_ne!(bound.connection_info.key, stale_info.key);
        assert_ne!(bound.connection_info.daemon_id, stale_info.daemon_id);
        assert!(bound
            .connection_info
            .endpoints
            .iter()
            .all(|endpoint| endpoint.host.parse::<IpAddr>().unwrap().is_loopback()));
        assert_owner_only_connection_file(&path);

        drop(bound.listeners);
    }

    #[tokio::test]
    async fn ensure_singleton_reclaims_invalid_connection_file_shapes() {
        let mut unsupported_schema = make_connection_info(8757);
        unsupported_schema.schema = SCHEMA_VERSION + 1;

        let mut empty_endpoints = make_connection_info(8757);
        empty_endpoints.endpoints.clear();

        let mut short_key = make_connection_info(8757);
        short_key.key = vec![0x5A; MIN_KEY_LEN - 1];

        let cases = vec![
            (
                "unsupported-schema",
                serde_json::to_vec(&unsupported_schema).unwrap(),
                Some(unsupported_schema),
            ),
            (
                "empty-endpoints",
                serde_json::to_vec(&empty_endpoints).unwrap(),
                Some(empty_endpoints),
            ),
            (
                "short-key",
                serde_json::to_vec(&short_key).unwrap(),
                Some(short_key),
            ),
            ("invalid-json", b"{not valid connection json".to_vec(), None),
        ];

        for (label, contents, old_info) in cases {
            let (_dir, path) = temp_connection_file_path(label);
            write_raw_owner_only_connection_file(&path, &contents);

            let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
            if let Some(old_info) = old_info {
                assert_ne!(bound.connection_info.key, old_info.key, "{label}");
                assert_ne!(
                    bound.connection_info.daemon_id, old_info.daemon_id,
                    "{label}"
                );
            }
            assert!(bound.connection_info.key.len() >= MIN_KEY_LEN, "{label}");
            assert_ne!(bound.connection_info.daemon_id, [0u8; 16], "{label}");
            assert_owner_only_connection_file(&path);

            drop(bound.listeners);
        }
    }

    #[tokio::test]
    async fn foreign_reused_port_connection_file_is_reclaimed_after_auth_probe_fails() {
        let (_dir, path) = temp_connection_file_path("foreign-reclaim");
        let foreign = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let foreign_port = foreign.local_addr().unwrap().port();
        write_atomic(&path, &make_connection_info(foreign_port)).unwrap();
        let foreign_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = foreign.accept().await {
                let mut buf = [0u8; 64];
                let _ = stream.read(&mut buf).await;
            }
        });

        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        assert!(bound
            .connection_info
            .endpoints
            .iter()
            .all(|endpoint| endpoint.port != foreign_port));

        drop(bound.listeners);
        let _ = foreign_task.await;
    }

    #[tokio::test]
    async fn stale_start_lock_file_is_reclaimable() {
        let (_dir, path) = temp_connection_file_path("start-lock-stale-file");
        let lock_path = start_lock_path(&path);
        drop(open_owner_only_lock(&lock_path).unwrap());
        assert!(lock_path.is_file());

        let lock = StartLock::acquire(&path).await.unwrap();
        assert!(lock_path.is_file());

        drop(lock);
        assert!(lock_path.is_file());
    }

    #[tokio::test]
    async fn held_start_lock_blocks_second_acquire_until_release() {
        let (_dir, path) = temp_connection_file_path("start-lock-held");
        let lock_path = start_lock_path(&path);
        let first = StartLock::acquire(&path).await.unwrap();

        let err = match StartLock::acquire(&path).await {
            Ok(_) => panic!("second acquire while held must stay busy"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            BootstrapError::StartLockBusy {
                ref path,
                attempts: START_LOCK_RETRIES,
            } if path == &lock_path
        ));

        drop(first);

        let second = StartLock::acquire(&path)
            .await
            .expect("released advisory lock should be reclaimable");
        drop(second);
    }

    #[tokio::test]
    async fn bind_conflict_on_fixed_port_fails_loud_without_reselecting() {
        let (_dir, path) = temp_connection_file_path("bind-conflict");
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();

        let err = ensure_singleton(&path, occupied_port).await.unwrap_err();
        assert!(matches!(
            err,
            BootstrapError::Bind { ref source, .. } if source.kind() == io::ErrorKind::AddrInUse
        ));
        assert!(err.to_string().contains("set the port in config"));

        drop(occupied);
    }

    #[tokio::test]
    async fn key_rotation_republishes_new_material_and_old_file_fails_auth() {
        let (_dir, path) = temp_connection_file_path("key-rotation");
        let first = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        let old_info = first.connection_info.clone();
        let fixed_port = old_info.endpoints[0].port;
        drop(first.listeners);

        let second = expect_bound(ensure_singleton(&path, fixed_port).await.unwrap());
        let new_info = second.connection_info.clone();
        assert_ne!(old_info.key, new_info.key);
        assert_ne!(old_info.daemon_id, new_info.daemon_id);
        let server = start_server(second);

        let mut old_stream = connect_from_info(&old_info).await.unwrap();
        let old_auth = authenticate_client(&mut old_stream, &old_info, PROBE_AUTH_DEADLINE).await;
        assert!(
            old_auth.is_err(),
            "old key must not authenticate after restart"
        );

        let reread = connection_file::read(&path).unwrap();
        let mut new_stream = connect_from_info(&reread).await.unwrap();
        authenticate_client(&mut new_stream, &reread, PROBE_AUTH_DEADLINE)
            .await
            .unwrap();

        server.abort();
        let _ = server.await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn published_connection_file_permissions_are_owner_only() {
        let (_dir, path) = temp_connection_file_path("permissions");
        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        drop(bound.listeners);
    }
}
