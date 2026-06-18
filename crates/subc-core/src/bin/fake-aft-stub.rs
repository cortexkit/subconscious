#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
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
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter},
    net::UnixStream,
    sync::mpsc,
    time::sleep,
};

const FAKE_AFT_MODULE_ID_ENV: &str = "FAKE_AFT_MODULE_ID";
const FAKE_AFT_CRASH_AFTER_MS_ENV: &str = "FAKE_AFT_CRASH_AFTER_MS";
const FAKE_AFT_REJECT_ATTACH_ENV: &str = "FAKE_AFT_REJECT_ATTACH";
const FAKE_AFT_EVENTS_PATH_ENV: &str = "FAKE_AFT_EVENTS_PATH";
const FAKE_AFT_EMIT_AFTER_DETACH_ENV: &str = "FAKE_AFT_EMIT_AFTER_DETACH";
const FAKE_AFT_PUSH_ON_REQUEST_ENV: &str = "FAKE_AFT_PUSH_ON_REQUEST";
const FAKE_AFT_FANOUT_ON_REQUEST_ENV: &str = "FAKE_AFT_FANOUT_ON_REQUEST";
const FAKE_AFT_DELAY_FROM_BODY_ENV: &str = "FAKE_AFT_DELAY_FROM_BODY";
const DEFAULT_MODULE_ID: &str = "fake-aft";
const HELLO_CORR: u64 = 1;
const STUB_EGRESS_BUFFER: usize = 64;

#[tokio::main]
async fn main() -> Result<(), StubError> {
    let config = StubConfig::from_env()?;
    run(config).await
}

async fn run(config: StubConfig) -> Result<(), StubError> {
    let stream = UnixStream::connect(&config.socket_path)
        .await
        .map_err(|source| StubError::Connect {
            path: config.socket_path.clone(),
            source,
        })?;
    let (mut read_half, write_half) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel::<Frame>(STUB_EGRESS_BUFFER);
    let writer = tokio::spawn(drain_writer(write_half, rx));

    let loop_result = module_loop(&mut read_half, tx.clone(), config).await;
    drop(tx);

    let writer_result = writer.await.map_err(StubError::WriterTask);
    match (loop_result, writer_result) {
        (Err(loop_err), _) => Err(loop_err),
        (Ok(()), Ok(Ok(()))) => Ok(()),
        (Ok(()), Ok(Err(writer_err))) => Err(StubError::FrameIo(writer_err)),
        (Ok(()), Err(join_err)) => Err(join_err),
    }
}

async fn module_loop<R>(
    read_half: &mut R,
    writer: mpsc::Sender<Frame>,
    config: StubConfig,
) -> Result<(), StubError>
where
    R: AsyncRead + Unpin,
{
    let mut state = StubState::default();

    send_hello(&writer, &config.module_id).await?;
    expect_hello_ack(read_half).await?;

    if let Some(crash_after) = config.crash_after {
        let crash = sleep(crash_after);
        tokio::pin!(crash);
        loop {
            tokio::select! {
                _ = &mut crash => {
                    std::process::exit(2);
                }
                frame = read_frame(read_half) => {
                    if !handle_frame(frame?, &config, &mut state, &writer).await? {
                        return Ok(());
                    }
                }
            }
        }
    }

    loop {
        let frame = read_frame(read_half).await?;
        if !handle_frame(frame, &config, &mut state, &writer).await? {
            return Ok(());
        }
    }
}

async fn drain_writer<W>(
    write_half: W,
    mut rx: mpsc::Receiver<Frame>,
) -> Result<(), subc_core::FrameIoError>
where
    W: AsyncWrite + Unpin,
{
    let mut writer = BufWriter::new(write_half);
    while let Some(frame) = rx.recv().await {
        write_frame(&mut writer, &frame).await?;
        while let Ok(frame) = rx.try_recv() {
            write_frame(&mut writer, &frame).await?;
        }
        writer.flush().await.map_err(subc_core::FrameIoError::Io)?;
    }
    writer.flush().await.map_err(subc_core::FrameIoError::Io)?;
    Ok(())
}

async fn send_hello(writer: &mpsc::Sender<Frame>, module_id: &str) -> Result<(), StubError> {
    let body = serde_json::to_vec(&HelloBody {
        manifest: manifest(module_id),
        protocol_ver: PROTOCOL_VERSION,
    })
    .map_err(StubError::Json)?;
    let frame = Frame::build(FrameType::Hello, control_flags(), 0, HELLO_CORR, body)
        .map_err(StubError::FrameBuild)?;
    send_outbound(writer, frame).await
}

async fn expect_hello_ack<R>(reader: &mut R) -> Result<HelloAckBody, StubError>
where
    R: AsyncRead + Unpin,
{
    let Some(frame) = read_frame(reader).await? else {
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
    frame: Option<Frame>,
    config: &StubConfig,
    state: &mut StubState,
    writer: &mpsc::Sender<Frame>,
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
            send_outbound(writer, pong).await?;
            Ok(true)
        }
        FrameType::Goodbye if frame.header.channel == 0 => Ok(false),
        FrameType::Request if frame.header.channel == 0 => {
            handle_control_request(frame, config, state, writer).await?;
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
            let delay = response_delay(config, &frame.body);
            let fanout_channels = state.bound_channels.iter().copied().collect::<Vec<_>>();
            let request_writer = writer.clone();
            let request_config = config.clone();

            if delay.is_zero() {
                handle_data_request(
                    request_writer,
                    frame,
                    request_config,
                    fanout_channels,
                    delay,
                )
                .await?;
            } else {
                tokio::spawn(async move {
                    let _ = handle_data_request(
                        request_writer,
                        frame,
                        request_config,
                        fanout_channels,
                        delay,
                    )
                    .await;
                });
            }

            Ok(true)
        }
        _ => Ok(true),
    }
}

async fn handle_data_request(
    writer: mpsc::Sender<Frame>,
    frame: Frame,
    config: StubConfig,
    fanout_channels: Vec<u16>,
    delay: Duration,
) -> Result<(), StubError> {
    if config.fanout_on_request {
        for channel in fanout_channels {
            send_push(&writer, frame.header.ver, channel).await?;
        }
    } else if config.push_on_request {
        send_push(&writer, frame.header.ver, frame.header.channel).await?;
    }

    if !delay.is_zero() {
        sleep(delay).await;
    }

    let response = Frame::build_with_version(
        frame.header.ver,
        FrameType::Response,
        frame.header.flags,
        frame.header.channel,
        frame.header.corr,
        frame.body,
    )
    .map_err(StubError::FrameBuild)?;
    send_outbound(&writer, response).await
}

async fn handle_control_request(
    frame: Frame,
    config: &StubConfig,
    state: &mut StubState,
    writer: &mpsc::Sender<Frame>,
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
            send_outbound(writer, response).await?;
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
        send_outbound(writer, response).await?;
        state.bound_channels.insert(route_channel);
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
    state.bound_channels.remove(&detach.route_channel);

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
        send_outbound(writer, stale).await?;
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

async fn send_push(
    writer: &mpsc::Sender<Frame>,
    version: u8,
    channel: u16,
) -> Result<(), StubError> {
    let push = Frame::build_with_version(
        version,
        FrameType::Push,
        Flags::new(false, Priority::Passive, true),
        channel,
        0,
        b"push-event".to_vec(),
    )
    .map_err(StubError::FrameBuild)?;
    send_outbound(writer, push).await
}

async fn send_outbound(writer: &mpsc::Sender<Frame>, frame: Frame) -> Result<(), StubError> {
    writer
        .send(frame)
        .await
        .map_err(|_| StubError::WriterClosed)
}

fn response_delay(config: &StubConfig, body: &[u8]) -> Duration {
    if !config.delay_from_body {
        return Duration::ZERO;
    }

    let delay_ms = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("delay_ms").and_then(Value::as_u64))
        .unwrap_or(0);
    Duration::from_millis(delay_ms)
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

#[derive(Debug, Clone)]
struct StubConfig {
    socket_path: PathBuf,
    module_id: String,
    crash_after: Option<Duration>,
    reject_attach: bool,
    events_path: Option<PathBuf>,
    emit_after_detach: bool,
    push_on_request: bool,
    fanout_on_request: bool,
    delay_from_body: bool,
}

#[derive(Debug, Default)]
struct StubState {
    bound_channels: BTreeSet<u16>,
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
            push_on_request: env_flag(FAKE_AFT_PUSH_ON_REQUEST_ENV),
            fanout_on_request: env_flag(FAKE_AFT_FANOUT_ON_REQUEST_ENV),
            delay_from_body: env_flag(FAKE_AFT_DELAY_FROM_BODY_ENV),
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
    WriterClosed,
    WriterTask(tokio::task::JoinError),
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
            Self::WriterClosed => write!(f, "module writer task closed"),
            Self::WriterTask(err) => write!(f, "module writer task failed: {err}"),
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
            Self::WriterTask(err) => Some(err),
            Self::MissingEnv { .. }
            | Self::WriterClosed
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
