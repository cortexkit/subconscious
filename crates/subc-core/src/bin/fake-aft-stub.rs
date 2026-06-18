use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fmt, fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{json, Value};
use subc_core::{
    read_frame, write_frame, AttachRelay, AttachRelayResponse, DetachRelay, Frame, HelloAckBody,
    HelloBody, SUBC_SOCKET_ENV,
};
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ConfigBinding, ConfigSource, IdentityBinding, IdentityScope,
        ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    ErrorBody, Flags, FrameType, Priority, PROTOCOL_VERSION,
};
use tokio::{io::AsyncWriteExt, net::UnixStream, time::sleep};

const FAKE_AFT_MODULE_ID_ENV: &str = "FAKE_AFT_MODULE_ID";
const FAKE_AFT_CRASH_AFTER_MS_ENV: &str = "FAKE_AFT_CRASH_AFTER_MS";
const FAKE_AFT_REJECT_ATTACH_ENV: &str = "FAKE_AFT_REJECT_ATTACH";
const FAKE_AFT_EVENTS_PATH_ENV: &str = "FAKE_AFT_EVENTS_PATH";
const FAKE_AFT_EMIT_AFTER_DETACH_ENV: &str = "FAKE_AFT_EMIT_AFTER_DETACH";
const DEFAULT_MODULE_ID: &str = "fake-aft";
const HELLO_CORR: u64 = 1;

#[tokio::main]
async fn main() -> Result<(), StubError> {
    let config = StubConfig::from_env()?;
    run(config).await
}

async fn run(config: StubConfig) -> Result<(), StubError> {
    let mut stream = UnixStream::connect(&config.socket_path)
        .await
        .map_err(|source| StubError::Connect {
            path: config.socket_path.clone(),
            source,
        })?;

    send_hello(&mut stream, &config.module_id).await?;
    expect_hello_ack(&mut stream).await?;

    if let Some(crash_after) = config.crash_after {
        let crash = sleep(crash_after);
        tokio::pin!(crash);
        loop {
            tokio::select! {
                _ = &mut crash => {
                    std::process::exit(2);
                }
                frame = read_frame(&mut stream) => {
                    if !handle_frame(&mut stream, frame?, &config).await? {
                        return Ok(());
                    }
                }
            }
        }
    }

    loop {
        let frame = read_frame(&mut stream).await?;
        if !handle_frame(&mut stream, frame, &config).await? {
            return Ok(());
        }
    }
}

async fn send_hello(stream: &mut UnixStream, module_id: &str) -> Result<(), StubError> {
    let body = serde_json::to_vec(&HelloBody {
        manifest: manifest(module_id),
        protocol_ver: PROTOCOL_VERSION,
    })
    .map_err(StubError::Json)?;
    let frame = Frame::build(FrameType::Hello, control_flags(), 0, HELLO_CORR, body)
        .map_err(StubError::FrameBuild)?;
    write_frame(stream, &frame).await?;
    stream.flush().await.map_err(StubError::Io)
}

async fn expect_hello_ack(stream: &mut UnixStream) -> Result<HelloAckBody, StubError> {
    let Some(frame) = read_frame(stream).await? else {
        return Err(StubError::ConnectionClosedBeforeHelloAck);
    };

    match frame.header.ty {
        FrameType::HelloAck => serde_json::from_slice(&frame.body).map_err(StubError::Json),
        FrameType::Error => {
            let body = serde_json::from_slice::<ErrorBody>(&frame.body).map_err(StubError::Json)?;
            Err(StubError::HelloRejected { body })
        }
        ty => Err(StubError::UnexpectedHelloAck { ty }),
    }
}

async fn handle_frame(
    stream: &mut UnixStream,
    frame: Option<Frame>,
    config: &StubConfig,
) -> Result<bool, StubError> {
    let Some(frame) = frame else {
        return Ok(false);
    };

    match frame.header.ty {
        FrameType::Ping if frame.header.channel == 0 => {
            let pong = Frame::build_with_version(
                frame.header.ver,
                FrameType::Pong,
                frame.header.flags,
                0,
                frame.header.corr,
                Vec::new(),
            )
            .map_err(StubError::FrameBuild)?;
            write_frame(stream, &pong).await?;
            stream.flush().await.map_err(StubError::Io)?;
            Ok(true)
        }
        FrameType::Goodbye if frame.header.channel == 0 => Ok(false),
        FrameType::Request if frame.header.channel == 0 => {
            handle_control_request(stream, frame, config).await?;
            Ok(true)
        }
        FrameType::Error => {
            let body = serde_json::from_slice::<ErrorBody>(&frame.body).ok();
            record_event(
                config,
                json!({
                    "kind": "error",
                    "channel": frame.header.channel,
                    "corr": frame.header.corr,
                    "code": body.as_ref().map(|body| body.code.as_str()),
                    "message": body.as_ref().map(|body| body.message.as_str()),
                }),
            )?;
            Ok(true)
        }
        FrameType::Request => {
            let response = Frame::build_with_version(
                frame.header.ver,
                FrameType::Response,
                frame.header.flags,
                frame.header.channel,
                frame.header.corr,
                frame.body,
            )
            .map_err(StubError::FrameBuild)?;
            write_frame(stream, &response).await?;
            stream.flush().await.map_err(StubError::Io)?;
            Ok(true)
        }
        _ => Ok(true),
    }
}

async fn handle_control_request(
    stream: &mut UnixStream,
    frame: Frame,
    config: &StubConfig,
) -> Result<(), StubError> {
    if let Ok(relay) = serde_json::from_slice::<AttachRelay>(&frame.body) {
        let route_channel = relay.route_channel;
        let relay_config = relay.config;
        record_event(
            config,
            json!({
                "kind": "attach",
                "route_channel": route_channel,
                "corr": frame.header.corr,
                "reject": config.reject_attach,
                "config": relay_config,
            }),
        )?;
        if config.reject_attach {
            let body = serde_json::to_vec(&ErrorBody {
                code: "config_divergence".to_string(),
                message: "fake AFT rejected AttachRelay by FAKE_AFT_REJECT_ATTACH".to_string(),
            })
            .map_err(StubError::Json)?;
            let response = Frame::build_with_version(
                frame.header.ver,
                FrameType::Error,
                control_flags(),
                0,
                frame.header.corr,
                body,
            )
            .map_err(StubError::FrameBuild)?;
            write_frame(stream, &response).await?;
            stream.flush().await.map_err(StubError::Io)?;
            return Ok(());
        }

        let body =
            serde_json::to_vec(&AttachRelayResponse { accept: true }).map_err(StubError::Json)?;
        let response = Frame::build_with_version(
            frame.header.ver,
            FrameType::Response,
            control_flags(),
            0,
            frame.header.corr,
            body,
        )
        .map_err(StubError::FrameBuild)?;
        write_frame(stream, &response).await?;
        stream.flush().await.map_err(StubError::Io)?;
        return Ok(());
    }

    let detach = serde_json::from_slice::<DetachRelay>(&frame.body).map_err(StubError::Json)?;
    record_event(
        config,
        json!({
            "kind": "detach",
            "route_channel": detach.route_channel,
            "corr": frame.header.corr,
        }),
    )?;

    if config.emit_after_detach {
        let stale = Frame::build_with_version(
            frame.header.ver,
            FrameType::Push,
            Flags::new(false, Priority::Passive, true),
            detach.route_channel,
            u64::from(detach.route_channel) + 9_000,
            b"stale-after-detach".to_vec(),
        )
        .map_err(StubError::FrameBuild)?;
        write_frame(stream, &stale).await?;
        stream.flush().await.map_err(StubError::Io)?;
        record_event(
            config,
            json!({
                "kind": "stale_emit",
                "route_channel": detach.route_channel,
            }),
        )?;
    }

    Ok(())
}

fn record_event(config: &StubConfig, event: Value) -> Result<(), StubError> {
    let Some(path) = config.events_path.as_ref() else {
        return Ok(());
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(StubError::Io)?;
    }
    append_json_line(path, event)
}

fn append_json_line(path: &Path, event: Value) -> Result<(), StubError> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(StubError::Io)?;
    writeln!(file, "{event}").map_err(StubError::Io)
}

fn manifest(module_id: &str) -> subc_protocol::manifest::ModuleManifest {
    subc_protocol::manifest::ModuleManifest {
        module_id: module_id.to_string(),
        module_version: "0.0.0-fake".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "fake_read".to_string(),
                mutates: false,
                schema: json!({"type": "object"}),
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: true,
            sub_supervises: true,
        }],
        consumes: Vec::new(),
        scheduled_tasks: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: true,
            },
            config: ConfigBinding {
                source: ConfigSource::SubcMediated,
                tiers: vec!["user".to_string(), "project".to_string()],
                expansion: BTreeMap::new(),
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
    }
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

#[derive(Debug)]
struct StubConfig {
    socket_path: PathBuf,
    module_id: String,
    crash_after: Option<Duration>,
    reject_attach: bool,
    events_path: Option<PathBuf>,
    emit_after_detach: bool,
}

impl StubConfig {
    fn from_env() -> Result<Self, StubError> {
        let socket_path = env::var_os(SUBC_SOCKET_ENV)
            .ok_or(StubError::MissingEnv {
                key: SUBC_SOCKET_ENV,
            })
            .map(PathBuf::from)?;
        let module_id = env::var(FAKE_AFT_MODULE_ID_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODULE_ID.to_string());
        let crash_after = env::var(FAKE_AFT_CRASH_AFTER_MS_ENV)
            .ok()
            .map(|raw| {
                raw.parse::<u64>()
                    .map(Duration::from_millis)
                    .map_err(|source| StubError::InvalidCrashAfter { raw, source })
            })
            .transpose()?;
        let events_path = env::var_os(FAKE_AFT_EVENTS_PATH_ENV).map(PathBuf::from);

        Ok(Self {
            socket_path,
            module_id,
            crash_after,
            reject_attach: env_flag(FAKE_AFT_REJECT_ATTACH_ENV),
            events_path,
            emit_after_detach: env_flag(FAKE_AFT_EMIT_AFTER_DETACH_ENV),
        })
    }
}

fn env_flag(key: &str) -> bool {
    matches!(
        env::var(key).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

#[derive(Debug)]
enum StubError {
    MissingEnv {
        key: &'static str,
    },
    InvalidCrashAfter {
        raw: String,
        source: std::num::ParseIntError,
    },
    Connect {
        path: PathBuf,
        source: io::Error,
    },
    Io(io::Error),
    FrameIo(subc_core::FrameIoError),
    FrameBuild(subc_core::FrameBuildError),
    Json(serde_json::Error),
    ConnectionClosedBeforeHelloAck,
    UnexpectedHelloAck {
        ty: FrameType,
    },
    HelloRejected {
        body: ErrorBody,
    },
}

impl fmt::Display for StubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv { key } => write!(f, "missing required env var {key}"),
            Self::InvalidCrashAfter { raw, source } => write!(
                f,
                "invalid {FAKE_AFT_CRASH_AFTER_MS_ENV} value '{raw}': {source}"
            ),
            Self::Connect { path, source } => {
                write!(
                    f,
                    "failed to connect to subc socket '{}': {source}",
                    path.display()
                )
            }
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::FrameIo(err) => write!(f, "frame I/O error: {err}"),
            Self::FrameBuild(err) => write!(f, "frame build error: {err}"),
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::ConnectionClosedBeforeHelloAck => {
                write!(f, "connection closed before HELLO_ACK")
            }
            Self::UnexpectedHelloAck { ty } => write!(f, "expected HELLO_ACK, got {ty:?}"),
            Self::HelloRejected { body } => write!(
                f,
                "HELLO rejected by subc: {} ({})",
                body.code, body.message
            ),
        }
    }
}

impl Error for StubError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidCrashAfter { source, .. } => Some(source),
            Self::Connect { source, .. } | Self::Io(source) => Some(source),
            Self::FrameIo(err) => Some(err),
            Self::FrameBuild(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::MissingEnv { .. }
            | Self::ConnectionClosedBeforeHelloAck
            | Self::UnexpectedHelloAck { .. }
            | Self::HelloRejected { .. } => None,
        }
    }
}

impl From<subc_core::FrameIoError> for StubError {
    fn from(err: subc_core::FrameIoError) -> Self {
        Self::FrameIo(err)
    }
}

impl From<io::Error> for StubError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}
