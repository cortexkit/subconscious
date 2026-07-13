use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::{ErrorBody, Flags, FrameType, Priority};
use subc_transport::{
    authenticate_client_with_role, connection_file, ConnectionFileError, ConnectionInfo,
    WATCHDOG_CLIENT_ROLE,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    task::JoinHandle,
    time::{self, Instant},
};
use tracing::{error, info};

use crate::{read_frame, write_frame, Frame};

pub const DEFAULT_SELF_WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);
pub const DEFAULT_SELF_WATCHDOG_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct DaemonSelfWatchdogConfig {
    interval: Duration,
    deadline: Duration,
}

impl Default for DaemonSelfWatchdogConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_SELF_WATCHDOG_INTERVAL,
            deadline: DEFAULT_SELF_WATCHDOG_DEADLINE,
        }
    }
}

impl DaemonSelfWatchdogConfig {
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn deadline(&self) -> Duration {
        self.deadline
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogStage {
    Connect,
    Authenticate,
    Describe,
    ConnectionFile,
    Timeout,
}

impl WatchdogStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Authenticate => "authenticate",
            Self::Describe => "describe",
            Self::ConnectionFile => "connection_file",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WatchdogTickError {
    stage: WatchdogStage,
    message: String,
}

impl WatchdogTickError {
    fn new(stage: WatchdogStage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }

    pub fn stage(&self) -> WatchdogStage {
        self.stage
    }
}

impl fmt::Display for WatchdogTickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WatchdogTickError {}

#[derive(Debug, Clone)]
pub struct DaemonSelfWatchdog {
    live_connection_info: ConnectionInfo,
    connection_file_path: PathBuf,
    config: DaemonSelfWatchdogConfig,
}

impl DaemonSelfWatchdog {
    pub fn new(
        live_connection_info: ConnectionInfo,
        connection_file_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            live_connection_info,
            connection_file_path: connection_file_path.into(),
            config: DaemonSelfWatchdogConfig::default(),
        }
    }

    pub fn with_config(mut self, config: DaemonSelfWatchdogConfig) -> Self {
        self.config = config;
        self
    }

    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    pub async fn run_once(&self) -> Result<(), WatchdogTickError> {
        self.verify_loopback().await?;
        self.verify_connection_file()?;
        Ok(())
    }

    async fn run(self) {
        let mut consecutive_failures = 0u64;
        let mut tick_index = 0u64;
        loop {
            time::sleep_until(
                Instant::now()
                    + jittered_watchdog_delay(
                        &self.live_connection_info,
                        tick_index,
                        self.config.interval(),
                    ),
            )
            .await;
            tick_index = tick_index.wrapping_add(1);

            let result = time::timeout(self.config.deadline(), self.run_once()).await;
            match result {
                Ok(Ok(())) => {
                    if consecutive_failures > 0 {
                        info!(
                            connection_file = %self.connection_file_path.display(),
                            failure_streak = consecutive_failures,
                            "daemon self-watchdog recovered"
                        );
                        consecutive_failures = 0;
                    }
                }
                Ok(Err(err)) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    error!(
                        connection_file = %self.connection_file_path.display(),
                        stage = err.stage().as_str(),
                        consecutive_failures,
                        error = %err,
                        "daemon self-watchdog tick failed"
                    );
                }
                Err(_) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    error!(
                        connection_file = %self.connection_file_path.display(),
                        stage = WatchdogStage::Timeout.as_str(),
                        consecutive_failures,
                        deadline_ms = self.config.deadline().as_millis(),
                        "daemon self-watchdog tick failed"
                    );
                }
            }
        }
    }

    async fn verify_loopback(&self) -> Result<(), WatchdogTickError> {
        let endpoint = self.live_connection_info.endpoints.first().ok_or_else(|| {
            WatchdogTickError::new(
                WatchdogStage::Describe,
                "live connection info has no endpoint",
            )
        })?;
        let ip = endpoint.host.parse::<IpAddr>().map_err(|err| {
            WatchdogTickError::new(
                WatchdogStage::Connect,
                format!(
                    "published endpoint host '{}' is not an IP: {err}",
                    endpoint.host
                ),
            )
        })?;
        let addr = SocketAddr::new(ip, endpoint.port);
        let mut stream = TcpStream::connect(addr).await.map_err(|err| {
            WatchdogTickError::new(WatchdogStage::Connect, format!("connect {addr}: {err}"))
        })?;

        authenticate_client_with_role(
            &mut stream,
            &self.live_connection_info,
            self.config.deadline(),
            WATCHDOG_CLIENT_ROLE,
        )
        .await
        .map_err(|err| {
            WatchdogTickError::new(
                WatchdogStage::Authenticate,
                format!("authenticate to {addr}: {err}"),
            )
        })?;

        let request = control_request_frame()?;
        write_frame(&mut stream, &request).await.map_err(|err| {
            WatchdogTickError::new(
                WatchdogStage::Describe,
                format!("write server.describe request to {addr}: {err}"),
            )
        })?;

        loop {
            let Some(reply) = read_frame(&mut stream).await.map_err(|err| {
                WatchdogTickError::new(
                    WatchdogStage::Describe,
                    format!("read server.describe reply from {addr}: {err}"),
                )
            })?
            else {
                return Err(WatchdogTickError::new(
                    WatchdogStage::Describe,
                    format!(
                        "daemon {addr} closed the connection before replying to server.describe"
                    ),
                ));
            };

            if reply.header.channel != 0 {
                continue;
            }
            match reply.header.ty {
                FrameType::Response => {
                    if reply.header.corr != request.header.corr {
                        return Err(WatchdogTickError::new(
                            WatchdogStage::Describe,
                            format!(
                                "server.describe reply correlation mismatch: expected {}, got {}",
                                request.header.corr, reply.header.corr
                            ),
                        ));
                    }
                    match serde_json::from_slice::<ClientControlResponse>(&reply.body) {
                        Ok(ClientControlResponse::ServerDescribe { .. }) => {
                            let _ = stream.shutdown().await;
                            return Ok(());
                        }
                        Ok(other) => {
                            return Err(WatchdogTickError::new(
                                WatchdogStage::Describe,
                                format!("unexpected server.describe reply: {other:?}"),
                            ));
                        }
                        Err(err) => {
                            return Err(WatchdogTickError::new(
                                WatchdogStage::Describe,
                                format!("decode server.describe reply: {err}"),
                            ));
                        }
                    }
                }
                FrameType::Error => {
                    return Err(WatchdogTickError::new(
                        WatchdogStage::Describe,
                        format!(
                            "server.describe rejected: {}",
                            decode_error_body(&reply.body)
                        ),
                    ));
                }
                _ => continue,
            }
        }
    }

    fn verify_connection_file(&self) -> Result<(), WatchdogTickError> {
        let file_info = connection_file::read(&self.connection_file_path)
            .map_err(|err| map_connection_file_error(&self.connection_file_path, err))?;

        let live_port = self
            .live_connection_info
            .endpoints
            .first()
            .map(|endpoint| endpoint.port)
            .ok_or_else(|| {
                WatchdogTickError::new(
                    WatchdogStage::ConnectionFile,
                    "live connection info has no endpoint",
                )
            })?;
        let file_ports = file_info
            .endpoints
            .iter()
            .map(|endpoint| endpoint.port)
            .collect::<BTreeSet<_>>();

        let mut divergences = Vec::new();
        if file_ports.len() != 1 || !file_ports.contains(&live_port) {
            divergences.push(format!(
                "port (live={live_port}, file={:?})",
                file_ports.into_iter().collect::<Vec<_>>()
            ));
        }
        if file_info.key != self.live_connection_info.key {
            divergences.push("key".to_owned());
        }

        if divergences.is_empty() {
            Ok(())
        } else {
            Err(WatchdogTickError::new(
                WatchdogStage::ConnectionFile,
                format!("connection file divergence: {}", divergences.join(", ")),
            ))
        }
    }
}

fn control_request_frame() -> Result<Frame, WatchdogTickError> {
    let body = serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).map_err(|err| {
        WatchdogTickError::new(
            WatchdogStage::Describe,
            format!("encode server.describe request: {err}"),
        )
    })?;
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0, // WIRE-WAVE2: thread the binding epoch.
        0,
        1,
        body,
    )
    .map_err(|err| {
        WatchdogTickError::new(
            WatchdogStage::Describe,
            format!("build server.describe request frame: {err}"),
        )
    })
}

fn decode_error_body(body: &[u8]) -> String {
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(error) => format!("{} — {}", error.code, error.message),
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    }
}

fn map_connection_file_error(path: &Path, err: ConnectionFileError) -> WatchdogTickError {
    let message = match err {
        ConnectionFileError::Io { op, source, .. } => {
            format!("connection file {} {}: {}", path.display(), op, source)
        }
        ConnectionFileError::JsonRead { source, .. } => {
            format!(
                "connection file {} parse failed: {}",
                path.display(),
                source
            )
        }
        ConnectionFileError::UnsupportedSchema { schema, supported } => format!(
            "connection file {} schema mismatch: file={}, supported={}",
            path.display(),
            schema,
            supported
        ),
        ConnectionFileError::Invalid { reason } => {
            format!("connection file {} invalid: {}", path.display(), reason)
        }
        ConnectionFileError::KeyTooShort { len, min } => format!(
            "connection file {} key is too short: len={}, min={}",
            path.display(),
            len,
            min
        ),
        ConnectionFileError::InsecurePermissions { mode, .. } => format!(
            "connection file {} permissions are not owner-only: mode={mode:#o}",
            path.display()
        ),
        other => format!("connection file {} error: {other}", path.display()),
    };
    WatchdogTickError::new(WatchdogStage::ConnectionFile, message)
}

fn jittered_watchdog_delay(
    live_connection_info: &ConnectionInfo,
    tick_index: u64,
    interval: Duration,
) -> Duration {
    if interval.is_zero() {
        return Duration::ZERO;
    }
    let interval_ms = interval.as_millis() as u64;
    if interval_ms == 0 {
        return interval;
    }

    let jitter_span = (interval_ms / 10).max(1);
    let hash = live_connection_info.daemon_id.iter().fold(
        tick_index.wrapping_mul(0x9E37_79B9_7F4A_7C15),
        |acc, byte| {
            acc.wrapping_mul(1099511628211)
                .wrapping_add(u64::from(*byte))
        },
    );
    let offset = (hash % (jitter_span.saturating_mul(2).saturating_add(1))) as i128
        - i128::from(jitter_span);
    let jittered_ms = (i128::from(interval_ms) + offset).max(0) as u64;
    Duration::from_millis(jittered_ms)
}

impl fmt::Display for DaemonSelfWatchdogConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "interval={:?}, deadline={:?}",
            self.interval(),
            self.deadline()
        )
    }
}

impl fmt::Display for WatchdogStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
