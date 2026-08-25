use std::{
    collections::BTreeMap,
    env,
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs4::{FileExt, TryLockError};
use subc_protocol::PROTOCOL_VERSION;
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

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub const DEFAULT_SUBC_PORT: u16 = 8757;
pub const SUBC_PORT_ENV: &str = "SUBC_PORT";
const CONNECTION_FILE_NAME: &str = "subc-connection.json";
const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_AUTH_DEADLINE: Duration = Duration::from_secs(2);
const START_LOCK_RETRIES: usize = 40;
const START_LOCK_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Runtime bootstrap configuration. Production uses the default fixed port and
/// optional daemon-config override; tests pass port 0 to let the OS assign a free
/// loopback port and discover it from the connection file.
#[derive(Debug, Clone, Default)]
struct AdmissionFactsConfig {
    carrier_module_id: Option<String>,
    targets: Option<Vec<String>>,
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
        }
    }

    pub fn from_env() -> Result<Self, BootstrapError> {
        Self::from_env_with_daemon_config_path(daemon_config::default_config_path())
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

        Ok(Self::new(connection_file_path(), port)
            .with_configured_modules(configured_modules)
            .with_storage_config(storage_config)
            .with_admission_facts_config(admission_facts_carrier_module_id, admission_facts_targets)
            .with_route_bind_relay_default_ms(route_bind_relay_default_ms)
            .with_reserved_capabilities(reserved_capabilities)
            .with_daemon_config_source(daemon_config_path, config_port))
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
}

/// Resolve subc's per-user TCP connection-file path.
///
/// `$XDG_RUNTIME_DIR/subc-connection.json` is preferred because the runtime
/// directory is already per-user on Unix desktops. Without it, subc falls back
/// to the system temp dir with a per-user token in the filename so different OS
/// users do not collide on shared temp directories.
pub fn connection_file_path() -> PathBuf {
    if let Some(runtime_dir) = non_empty_os_var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join(CONNECTION_FILE_NAME);
    }

    env::temp_dir().join(format!("subc-{}.connection.json", user_connection_token()))
}

/// Resolve, claim, and serve the per-user daemon singleton.
///
/// A second invocation is successful: if a live daemon authenticates from the
/// existing connection file, this returns `Ok(())` after logging and the caller
/// exits with status 0.
pub async fn run() -> Result<(), BootstrapError> {
    run_with_config(BootstrapConfig::from_env()?).await
}

pub async fn run_with_config(config: BootstrapConfig) -> Result<(), BootstrapError> {
    let configured_modules = config.configured_modules.clone();
    let storage_config = config.storage_config.clone();
    let admission_facts = config.admission_facts.clone();
    let daemon_config_path = config.daemon_config_path.clone();
    let configured_port = config.configured_port;
    let route_bind_relay_default_ms = config.route_bind_relay_default_ms;
    let reserved_capabilities = config.reserved_capabilities.clone();
    let watchdog_config = config.watchdog_config.clone();
    match ensure_singleton_with_config(config).await? {
        Outcome::AlreadyRunning => {
            info!("subc daemon already running");
            Ok(())
        }
        Outcome::Bound(bound) => {
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
            )
            .await
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
) -> Result<(), BootstrapError> {
    raise_nofile_limit();

    info!(
        connection_file = %bound.connection_file_path.display(),
        endpoints = ?bound.connection_info.endpoints,
        configured_modules = configured_modules.len(),
        "subc daemon starting"
    );

    let registry = Arc::new(Registry::default());
    let process_liveness = Arc::new(SupervisorProcessLiveness::new());
    let supervisor_handle = SupervisorHandle::new();
    let connected_clients = ConnectedClients::new();
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = Supervisor::new(Arc::clone(&registry), RestartPolicy::default())
        .with_process_liveness(process_liveness.clone())
        .with_forwarding(Arc::clone(&forwarding))
        .with_handle(supervisor_handle.clone())
        .with_connection_file_path(bound.connection_file_path.clone());
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
    let control_started_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let mut control = ControlHandler::with_forwarding(Arc::clone(&registry), forwarding)
        .with_process_liveness(process_liveness)
        .with_supervisor(supervisor_handle)
        .with_connected_clients(connected_clients.clone())
        .with_storage_config(storage_config)
        .with_admission_facts_config(admission_facts.carrier_module_id, admission_facts.targets)
        .with_route_bind_relay_timeouts(route_bind_relay_timeouts)
        .with_daemon_provenance(
            bound.connection_info.pid,
            control_started_at_ms,
            std::env::current_exe().ok(),
            normalized_build_provenance(env!("SUBC_BUILD_GIT_SHA")),
            normalized_build_provenance(env!("SUBC_BUILD_LOCK_DIGEST")),
        )
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

/// The per-user component of the connection-file name.
///
/// Public because `ck` derives the same token when discovering which daemon to
/// talk to. The daemon writes the file and the CLI finds it, so the two must
/// agree by construction: a second copy of this logic that drifted by one
/// character would send `ck` looking for a file the daemon never wrote, and the
/// symptom would be "no daemon running" rather than anything pointing at a
/// naming mismatch.
pub fn user_connection_token() -> String {
    #[cfg(unix)]
    if let Some(uid) = unix_uid_token() {
        return uid;
    }

    for key in ["USER", "USERNAME", "HOME", "USERPROFILE"] {
        if let Some(value) = non_empty_os_var(key) {
            return sanitize_token(&value.to_string_lossy());
        }
    }

    "unknown".to_string()
}

#[cfg(unix)]
fn unix_uid_token() -> Option<String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let probe_path = env::temp_dir().join(format!(".subc-uid-probe-{}-{nonce}", process::id()));

    let uid = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe_path)
        .ok()
        .and_then(|file| {
            let uid = file.metadata().ok().map(|metadata| metadata.uid());
            drop(file);
            let _ = fs::remove_file(&probe_path);
            uid
        });

    uid.map(|uid| uid.to_string())
}

fn sanitize_token(raw: &str) -> String {
    let mut token = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            token.push(ch);
        } else {
            token.push('_');
        }
    }
    if token.is_empty() {
        "unknown".to_string()
    } else {
        token
    }
}

/// Bootstrap-layer errors are deliberately typed so startup never panics for
/// ordinary daemon-discovery races or stale filesystem state.
#[derive(Debug)]
pub enum BootstrapError {
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
    Serve(ServerError),
    ServeJoin(tokio::task::JoinError),
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            Self::Serve(err) => write!(f, "daemon server failed: {err}"),
            Self::ServeJoin(err) => write!(f, "daemon server task failed: {err}"),
        }
    }
}

impl Error for BootstrapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidPort { source, .. } => Some(source),
            Self::ConnectionFileRead { source, .. }
            | Self::ConnectionFileWrite { source, .. }
            | Self::GenerateConnectionFile(source) => Some(source),
            Self::StartLockCreate { source, .. }
            | Self::RemoveStale { source, .. }
            | Self::Bind { source, .. }
            | Self::LocalAddr { source, .. } => Some(source),
            Self::DaemonConfig(err) => Some(err),
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
    use std::sync::Mutex;
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

    fn unique_temp_dir(name: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("subc-core-{name}-{}-{nonce}", process::id()))
    }

    fn temp_connection_file_path(name: &str) -> PathBuf {
        let dir = unique_temp_dir(name);
        fs::create_dir_all(&dir).unwrap();
        dir.join("conn.json")
    }

    fn cleanup_connection_file_path(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(start_lock_path(path));
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
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
        fs::create_dir_all(&runtime_dir).unwrap();
        let _xdg = EnvGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

        assert_eq!(
            connection_file_path(),
            runtime_dir.join(CONNECTION_FILE_NAME)
        );

        let _ = fs::remove_dir_all(runtime_dir);
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
    fn configured_port_uses_default_config_and_env_override() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let config_path =
            temp_connection_file_path("daemon-config-port").with_file_name("subc.jsonc");

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

        cleanup_connection_file_path(&config_path);
    }

    #[tokio::test]
    async fn second_singleton_probe_against_served_tcp_daemon_reports_already_running() {
        let path = temp_connection_file_path("already-running");

        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        let server = start_server(bound);

        let second = ensure_singleton(&path, 0).await.unwrap();
        assert!(matches!(second, Outcome::AlreadyRunning));

        server.abort();
        let _ = server.await;
        cleanup_connection_file_path(&path);
    }

    #[tokio::test]
    async fn daemon_connection_file_publishes_protocol_wire_version() {
        let path = temp_connection_file_path("wire-version");
        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        assert_eq!(bound.connection_info.wire_version, Some(PROTOCOL_VERSION));
        assert_eq!(
            connection_file::read(&path).unwrap().wire_version,
            Some(PROTOCOL_VERSION)
        );

        drop(bound.listeners);
        cleanup_connection_file_path(&path);
    }

    #[tokio::test]
    async fn stale_unbound_connection_file_is_reclaimed() {
        let path = temp_connection_file_path("stale-reclaim");
        let stale = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let stale_port = stale.local_addr().unwrap().port();
        drop(stale);
        let stale_info = make_connection_info(stale_port);
        write_atomic(&path, &stale_info).unwrap();

        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());
        assert_ne!(bound.connection_info.key, stale_info.key);
        drop(bound.listeners);
        cleanup_connection_file_path(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_singleton_reclaims_insecure_connection_file() {
        let path = temp_connection_file_path("insecure-reclaim");
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
        cleanup_connection_file_path(&path);
    }

    #[tokio::test]
    async fn ensure_singleton_reclaims_non_loopback_connection_file() {
        let path = temp_connection_file_path("non-loopback-reclaim");
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
        cleanup_connection_file_path(&path);
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
            let path = temp_connection_file_path(label);
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
            cleanup_connection_file_path(&path);
        }
    }

    #[tokio::test]
    async fn foreign_reused_port_connection_file_is_reclaimed_after_auth_probe_fails() {
        let path = temp_connection_file_path("foreign-reclaim");
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
        cleanup_connection_file_path(&path);
    }

    #[tokio::test]
    async fn stale_start_lock_file_is_reclaimable() {
        let path = temp_connection_file_path("start-lock-stale-file");
        let lock_path = start_lock_path(&path);
        drop(open_owner_only_lock(&lock_path).unwrap());
        assert!(lock_path.is_file());

        let lock = StartLock::acquire(&path).await.unwrap();
        assert!(lock_path.is_file());

        drop(lock);
        assert!(lock_path.is_file());
        cleanup_connection_file_path(&path);
    }

    #[tokio::test]
    async fn held_start_lock_blocks_second_acquire_until_release() {
        let path = temp_connection_file_path("start-lock-held");
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
        cleanup_connection_file_path(&path);
    }

    #[tokio::test]
    async fn bind_conflict_on_fixed_port_fails_loud_without_reselecting() {
        let path = temp_connection_file_path("bind-conflict");
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();

        let err = ensure_singleton(&path, occupied_port).await.unwrap_err();
        assert!(matches!(
            err,
            BootstrapError::Bind { ref source, .. } if source.kind() == io::ErrorKind::AddrInUse
        ));
        assert!(err.to_string().contains("set the port in config"));

        drop(occupied);
        cleanup_connection_file_path(&path);
    }

    #[tokio::test]
    async fn key_rotation_republishes_new_material_and_old_file_fails_auth() {
        let path = temp_connection_file_path("key-rotation");
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
        cleanup_connection_file_path(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn published_connection_file_permissions_are_owner_only() {
        let path = temp_connection_file_path("permissions");
        let bound = expect_bound(ensure_singleton(&path, 0).await.unwrap());

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        drop(bound.listeners);
        cleanup_connection_file_path(&path);
    }
}
