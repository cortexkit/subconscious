#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env,
    error::Error,
    ffi::OsString,
    fmt, fs,
    io::{self, Write as _},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{json, Value};
use subc_core::{read_frame, write_frame, Frame};
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ConfigBinding, ConfigSource, IdentityBinding, IdentityScope,
        InternalTransport, ManagementOperation, ManagementOperationKind, ObservabilityKind,
        ObservabilitySurface, PipelineAppliesTo, PipelineStageKind, ProviderRole, StorageBinding,
        StorageKind, StorageScope, Tool, TrustTier,
    },
    session::{ModuleControlPush, ModuleControlRequest, ModuleControlResponse},
    ErrorBody, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority, PROTOCOL_VERSION,
};
use subc_transport::{authenticate_client, connection_file, AuthError, ConnectionFileError};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter},
    net::TcpStream,
    sync::{mpsc, oneshot},
    time::sleep,
};

const FAKE_AFT_MODULE_ID_ENV: &str = "FAKE_AFT_MODULE_ID";
const FAKE_AFT_CRASH_AFTER_MS_ENV: &str = "FAKE_AFT_CRASH_AFTER_MS";
const FAKE_AFT_REJECT_ATTACH_ENV: &str = "FAKE_AFT_REJECT_ATTACH";
const FAKE_AFT_FAIL_REGISTRATION_ENV: &str = "FAKE_AFT_FAIL_REGISTRATION";
const FAKE_AFT_FAIL_REGISTRATION_AFTER_FIRST_PATH_ENV: &str =
    "FAKE_AFT_FAIL_REGISTRATION_AFTER_FIRST_PATH";
const FAKE_AFT_EVENTS_PATH_ENV: &str = "FAKE_AFT_EVENTS_PATH";
const FAKE_AFT_EMIT_AFTER_DETACH_ENV: &str = "FAKE_AFT_EMIT_AFTER_DETACH";
const FAKE_AFT_PUSH_ON_REQUEST_ENV: &str = "FAKE_AFT_PUSH_ON_REQUEST";
const FAKE_AFT_FANOUT_ON_REQUEST_ENV: &str = "FAKE_AFT_FANOUT_ON_REQUEST";
const FAKE_AFT_DELAY_FROM_BODY_ENV: &str = "FAKE_AFT_DELAY_FROM_BODY";
const FAKE_AFT_CONCURRENCY_ENV: &str = "FAKE_AFT_CONCURRENCY";
const FAKE_AFT_DOUBLE_TERMINAL_ENV: &str = "FAKE_AFT_DOUBLE_TERMINAL";
const FAKE_AFT_STATUS_ENV: &str = "FAKE_AFT_STATUS";
const FAKE_AFT_ROLE_ENV: &str = "FAKE_AFT_ROLE";
const FAKE_AFT_SERVICE_ID_ENV: &str = "FAKE_AFT_SERVICE_ID";
const FAKE_AFT_TOOLCALL_PROGRESS_ENV: &str = "FAKE_AFT_TOOLCALL_PROGRESS";
const FAKE_AFT_TOOLCALL_DELAY_MS_ENV: &str = "FAKE_AFT_TOOLCALL_DELAY_MS";
const FAKE_AFT_TOOLCALL_RESULT_ENV: &str = "FAKE_AFT_TOOLCALL_RESULT";
const FAKE_AFT_TOOLCALL_ERROR_ENV: &str = "FAKE_AFT_TOOLCALL_ERROR";
const FAKE_AFT_TOOLCALL_SUBC_ERROR_ENV: &str = "FAKE_AFT_TOOLCALL_SUBC_ERROR";
const FAKE_AFT_TOOLS_ENV: &str = "FAKE_AFT_TOOLS";
const DEFAULT_MODULE_ID: &str = "fake-aft";
const HELLO_CORR: u64 = 1;
const STUB_EGRESS_BUFFER: usize = 64;

type InFlightKey = (u16, u64);
type InFlightRegistry = Arc<Mutex<HashMap<InFlightKey, oneshot::Sender<()>>>>;

#[tokio::main]
async fn main() -> Result<(), StubError> {
    let config = StubConfig::from_env()?;
    run(config).await
}

async fn run(config: StubConfig) -> Result<(), StubError> {
    if config.fail_registration {
        std::process::exit(2);
    }

    let stream = connect_to_subc(&config.connection_file_path).await?;
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

async fn connect_to_subc(connection_file_path: &Path) -> Result<TcpStream, StubError> {
    // follow-up (4.3): future reconnect loops must call this helper for every
    // reconnect so key rotation is observed by re-reading the connection file.
    let conn = connection_file::read(connection_file_path).map_err(|source| {
        StubError::ConnectionFile {
            path: connection_file_path.to_path_buf(),
            source,
        }
    })?;
    let endpoint = conn
        .endpoints
        .first()
        .ok_or_else(|| StubError::NoEndpoint {
            path: connection_file_path.to_path_buf(),
        })?;
    let endpoint_label = format!("{}:{}", endpoint.host, endpoint.port);
    let ip = endpoint
        .host
        .parse::<IpAddr>()
        .map_err(|_| StubError::InvalidEndpoint {
            path: connection_file_path.to_path_buf(),
            endpoint: endpoint_label.clone(),
        })?;
    let addr = SocketAddr::new(ip, endpoint.port);
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|source| StubError::Connect {
            path: connection_file_path.to_path_buf(),
            endpoint: endpoint_label.clone(),
            source,
        })?;
    authenticate_client(&mut stream, &conn, Duration::from_secs(2))
        .await
        .map_err(|source| StubError::Auth {
            path: connection_file_path.to_path_buf(),
            endpoint: endpoint_label,
            source,
        })?;
    Ok(stream)
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

    send_hello(&writer, &config).await?;
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

async fn send_hello(writer: &mpsc::Sender<Frame>, config: &StubConfig) -> Result<(), StubError> {
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest: manifest(
            &config.module_id,
            config.role.clone(),
            config.concurrency.clone(),
            &config.tools,
        ),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
    })
    .map_err(StubError::Json)?;
    let frame = Frame::build(FrameType::Hello, control_flags(), 0, HELLO_CORR, body)
        .map_err(StubError::FrameBuild)?;
    send_outbound(writer, frame).await
}

async fn expect_hello_ack<R>(reader: &mut R) -> Result<ModuleHelloAckBody, StubError>
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
        FrameType::Goodbye => {
            handle_route_goodbye(frame, config, state, writer).await?;
            Ok(true)
        }
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
        FrameType::Cancel => {
            handle_cancel(frame, config, state, writer).await?;
            Ok(true)
        }
        FrameType::Request => {
            record_event(
                config,
                json!({
                    "kind": "request_received",
                    "channel": frame.header.channel,
                    "corr": frame.header.corr,
                    "body_json": serde_json::from_slice::<Value>(&frame.body).ok(),
                }),
            )?;
            let behavior = request_behavior(config, &frame.body);
            let fanout_channels = state.bound_channels.iter().copied().collect::<Vec<_>>();
            let request_writer = writer.clone();
            let request_config = config.clone();

            if behavior.delay.is_zero() {
                handle_data_request(
                    request_writer,
                    frame,
                    request_config,
                    fanout_channels,
                    behavior.delay,
                )
                .await?;
            } else if behavior.cancellable {
                let key = (frame.header.channel, frame.header.corr);
                let (cancel_tx, cancel_rx) = oneshot::channel();
                {
                    let mut in_flight = lock_in_flight(&state.in_flight)?;
                    in_flight.insert(key, cancel_tx);
                }
                let in_flight = Arc::clone(&state.in_flight);
                tokio::spawn(async move {
                    let _ = handle_cancellable_data_request(
                        request_writer,
                        frame,
                        request_config,
                        fanout_channels,
                        behavior.delay,
                        in_flight,
                        cancel_rx,
                    )
                    .await;
                });
            } else {
                tokio::spawn(async move {
                    let _ = handle_data_request(
                        request_writer,
                        frame,
                        request_config,
                        fanout_channels,
                        behavior.delay,
                    )
                    .await;
                });
            }

            Ok(true)
        }
        _ => Ok(true),
    }
}

async fn handle_cancel(
    frame: Frame,
    config: &StubConfig,
    state: &StubState,
    writer: &mpsc::Sender<Frame>,
) -> Result<(), StubError> {
    let key = (frame.header.channel, frame.header.corr);
    let cancel_tx = {
        let mut in_flight = lock_in_flight(&state.in_flight)?;
        in_flight.remove(&key)
    };
    let claimed = cancel_tx.is_some();

    record_event(
        config,
        json!({
            "kind": "cancel",
            "channel": frame.header.channel,
            "corr": frame.header.corr,
            "claimed": claimed,
        }),
    )?;

    if let Some(cancel_tx) = cancel_tx {
        // Story 2.4 resolves the flow-control interaction: CANCEL bypasses
        // request credits; the request credit returns only on this terminal.
        let _ = cancel_tx.send(());
        emit_cancelled_error(
            writer,
            config,
            frame.header.ver,
            frame.header.channel,
            frame.header.corr,
        )
        .await?;
    }

    Ok(())
}

async fn handle_data_request(
    writer: mpsc::Sender<Frame>,
    frame: Frame,
    config: StubConfig,
    fanout_channels: Vec<u16>,
    delay: Duration,
) -> Result<(), StubError> {
    send_requested_pushes(
        &writer,
        &config,
        frame.header.ver,
        frame.header.channel,
        &fanout_channels,
    )
    .await?;

    if config.toolcall_progress && parse_tool_call(&frame.body).is_some() {
        emit_tool_call_progress(
            &writer,
            &config,
            frame.header.ver,
            frame.header.channel,
            frame.header.corr,
        )
        .await?;
    }

    if !delay.is_zero() {
        sleep(delay).await;
    }

    emit_response(&writer, &config, frame).await
}

async fn handle_cancellable_data_request(
    writer: mpsc::Sender<Frame>,
    frame: Frame,
    config: StubConfig,
    fanout_channels: Vec<u16>,
    delay: Duration,
    in_flight: InFlightRegistry,
    cancel_rx: oneshot::Receiver<()>,
) -> Result<(), StubError> {
    let key = (frame.header.channel, frame.header.corr);
    send_requested_pushes(
        &writer,
        &config,
        frame.header.ver,
        frame.header.channel,
        &fanout_channels,
    )
    .await?;

    if config.toolcall_progress && parse_tool_call(&frame.body).is_some() {
        emit_tool_call_progress(
            &writer,
            &config,
            frame.header.ver,
            frame.header.channel,
            frame.header.corr,
        )
        .await?;
    }

    tokio::select! {
        _ = sleep(delay) => {}
        _ = cancel_rx => return Ok(()),
    }

    let claimed = {
        let mut in_flight = lock_in_flight(&in_flight)?;
        in_flight.remove(&key).is_some()
    };
    if !claimed {
        return Ok(());
    }

    emit_response(&writer, &config, frame).await
}

async fn send_requested_pushes(
    writer: &mpsc::Sender<Frame>,
    config: &StubConfig,
    version: u8,
    request_channel: u16,
    fanout_channels: &[u16],
) -> Result<(), StubError> {
    if config.fanout_on_request {
        for channel in fanout_channels {
            send_push(writer, version, *channel).await?;
        }
    } else if config.push_on_request {
        send_push(writer, version, request_channel).await?;
    }

    Ok(())
}

async fn emit_response(
    writer: &mpsc::Sender<Frame>,
    config: &StubConfig,
    frame: Frame,
) -> Result<(), StubError> {
    let channel = frame.header.channel;
    let corr = frame.header.corr;
    let body = if let Some(tool_call) = parse_tool_call(&frame.body) {
        record_event(
            config,
            json!({
                "kind": "tool_call",
                "channel": channel,
                "corr": corr,
                "name": tool_call.name.clone(),
                "arguments": tool_call.arguments.clone(),
                "progress_token": tool_call.progress_token.clone(),
            }),
        )?;
        if config.toolcall_subc_error {
            return emit_tool_call_subc_error(writer, config, frame.header.ver, channel, corr)
                .await;
        }
        tool_call_response_body(config, &tool_call)?
    } else {
        frame.body
    };
    let response = Frame::build_with_version(
        frame.header.ver,
        FrameType::Response,
        frame.header.flags,
        channel,
        corr,
        body,
    )
    .map_err(StubError::FrameBuild)?;
    send_outbound(writer, response.clone()).await?;
    record_terminal(config, "response", None, channel, corr)?;
    if config.double_terminal {
        send_outbound(writer, response).await?;
        record_terminal(config, "response", None, channel, corr)?;
    }
    Ok(())
}

async fn emit_cancelled_error(
    writer: &mpsc::Sender<Frame>,
    config: &StubConfig,
    version: u8,
    channel: u16,
    corr: u64,
) -> Result<(), StubError> {
    let body = serde_json::to_vec(&ErrorBody {
        code: "cancelled".to_string(),
        message: "request cancelled by client".to_string(),
    })
    .map_err(StubError::Json)?;
    let frame = Frame::build_with_version(
        version,
        FrameType::Error,
        Flags::new(false, Priority::Passive, false),
        channel,
        corr,
        body,
    )
    .map_err(StubError::FrameBuild)?;
    send_outbound(writer, frame.clone()).await?;
    record_terminal(config, "error", Some("cancelled"), channel, corr)?;
    if config.double_terminal {
        send_outbound(writer, frame).await?;
        record_terminal(config, "error", Some("cancelled"), channel, corr)?;
    }
    Ok(())
}

fn record_terminal(
    config: &StubConfig,
    terminal: &str,
    code: Option<&str>,
    channel: u16,
    corr: u64,
) -> Result<(), StubError> {
    let mut event = json!({
        "kind": "terminal",
        "terminal": terminal,
        "channel": channel,
        "corr": corr,
    });
    if let Some(code) = code {
        event["code"] = json!(code);
    }
    record_event(config, event)
}

async fn handle_control_request(
    frame: Frame,
    config: &StubConfig,
    state: &mut StubState,
    writer: &mpsc::Sender<Frame>,
) -> Result<(), StubError> {
    let request =
        serde_json::from_slice::<ModuleControlRequest>(&frame.body).map_err(StubError::Json)?;
    match request {
        ModuleControlRequest::RouteBind {
            route_channel,
            target,
            identity,
            config: relay_config,
        } => {
            record_event(
                config,
                json!({
                    "kind": "attach",
                    "route_channel": route_channel,
                    "corr": frame.header.corr,
                    "reject": config.reject_attach,
                    "target": target,
                    "identity": identity,
                    "config": relay_config,
                }),
            )?;
            if config.reject_attach {
                let body = serde_json::to_vec(&ErrorBody {
                    code: "config_divergence".to_string(),
                    message: "fake AFT rejected route.bind by FAKE_AFT_REJECT_ATTACH".to_string(),
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

            let body = serde_json::to_vec(&ModuleControlResponse::RouteBindAck {})
                .map_err(StubError::Json)?;
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
            emit_status_update(writer, config, frame.header.ver, route_channel).await?;
        }
    }
    Ok(())
}

async fn handle_route_goodbye(
    frame: Frame,
    config: &StubConfig,
    state: &mut StubState,
    writer: &mpsc::Sender<Frame>,
) -> Result<(), StubError> {
    let route_channel = frame.header.channel;
    record_event(
        config,
        json!({
            "kind": "detach",
            "route_channel": route_channel,
            "corr": frame.header.corr,
        }),
    )?;
    state.bound_channels.remove(&route_channel);

    if config.emit_after_detach {
        let stale = Frame::build_with_version(
            frame.header.ver,
            FrameType::Push,
            Flags::new(false, Priority::Passive, true),
            route_channel,
            u64::from(route_channel) + 9_000,
            b"stale-after-detach".to_vec(),
        )
        .map_err(StubError::FrameBuild)?;
        send_outbound(writer, stale).await?;
        record_event(
            config,
            json!({
                "kind": "stale_emit",
                "route_channel": route_channel,
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

async fn emit_tool_call_progress(
    writer: &mpsc::Sender<Frame>,
    config: &StubConfig,
    version: u8,
    channel: u16,
    corr: u64,
) -> Result<(), StubError> {
    let body = serde_json::to_vec(&json!({
        "progress": 1.0,
        "total": 2.0,
        "message": "fake-aft progress",
    }))
    .map_err(StubError::Json)?;
    let push = Frame::build_with_version(
        version,
        FrameType::Push,
        Flags::new(false, Priority::Passive, true),
        channel,
        corr,
        body,
    )
    .map_err(StubError::FrameBuild)?;
    send_outbound(writer, push).await?;
    record_event(
        config,
        json!({
            "kind": "tool_progress",
            "channel": channel,
            "corr": corr,
        }),
    )
}

async fn emit_tool_call_subc_error(
    writer: &mpsc::Sender<Frame>,
    config: &StubConfig,
    version: u8,
    channel: u16,
    corr: u64,
) -> Result<(), StubError> {
    let body = serde_json::to_vec(&ErrorBody {
        code: "target_unavailable".to_string(),
        message: "fake AFT injected subc-level tool-call failure".to_string(),
    })
    .map_err(StubError::Json)?;
    let frame = Frame::build_with_version(
        version,
        FrameType::Error,
        Flags::new(false, Priority::Passive, false),
        channel,
        corr,
        body,
    )
    .map_err(StubError::FrameBuild)?;
    send_outbound(writer, frame.clone()).await?;
    record_terminal(config, "error", Some("target_unavailable"), channel, corr)?;
    if config.double_terminal {
        send_outbound(writer, frame).await?;
        record_terminal(config, "error", Some("target_unavailable"), channel, corr)?;
    }
    Ok(())
}

fn tool_call_response_body(
    config: &StubConfig,
    tool_call: &ToolCallRouteRequest,
) -> Result<Vec<u8>, StubError> {
    let is_error = config.toolcall_error;
    let text = config.toolcall_result.clone().unwrap_or_else(|| {
        if is_error {
            format!("fake-aft tool error: {}", tool_call.name)
        } else {
            format!(
                "fake-aft tool {} called with {}",
                tool_call.name, tool_call.arguments
            )
        }
    });
    serde_json::to_vec(&json!({
        "content": [
            {
                "type": "text",
                "text": text,
            }
        ],
        "isError": is_error,
    }))
    .map_err(StubError::Json)
}

async fn emit_status_update(
    writer: &mpsc::Sender<Frame>,
    config: &StubConfig,
    version: u8,
    route_channel: u16,
) -> Result<(), StubError> {
    let Some(status) = config.status.as_ref() else {
        return Ok(());
    };
    let body = serde_json::to_vec(&ModuleControlPush::RouteStatus {
        route_channel,
        status: status.clone(),
    })
    .map_err(StubError::Json)?;
    let push = Frame::build_with_version(version, FrameType::Push, control_flags(), 0, 0, body)
        .map_err(StubError::FrameBuild)?;
    send_outbound(writer, push).await?;
    record_event(
        config,
        json!({
            "kind": "status_published",
            "route_channel": route_channel,
            "status": status,
        }),
    )
}

async fn send_outbound(writer: &mpsc::Sender<Frame>, frame: Frame) -> Result<(), StubError> {
    writer
        .send(frame)
        .await
        .map_err(|_| StubError::WriterClosed)
}

#[derive(Debug, Clone, Copy)]
struct RequestBehavior {
    delay: Duration,
    cancellable: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolCallRouteRequest {
    name: String,
    #[serde(default)]
    arguments: Value,
    #[serde(default)]
    progress_token: Option<Value>,
}

fn request_behavior(config: &StubConfig, body: &[u8]) -> RequestBehavior {
    if let Some(tool_call) = parse_tool_call(body) {
        let delay = if config.toolcall_delay.is_zero() {
            tool_call
                .arguments
                .get("delay_ms")
                .and_then(Value::as_u64)
                .map(Duration::from_millis)
                .unwrap_or(Duration::ZERO)
        } else {
            config.toolcall_delay
        };
        return RequestBehavior {
            delay,
            cancellable: true,
        };
    }

    if !config.delay_from_body {
        return RequestBehavior {
            delay: Duration::ZERO,
            cancellable: true,
        };
    }

    // Test-only body flag: {"uncancellable": true, "delay_ms": N} models
    // mutation-like work that ignores CANCEL and completes normally.
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let delay_ms = parsed
        .as_ref()
        .and_then(|value| value.get("delay_ms").and_then(Value::as_u64))
        .unwrap_or(0);
    let uncancellable = parsed
        .as_ref()
        .and_then(|value| value.get("uncancellable").and_then(Value::as_bool))
        .unwrap_or(false);

    RequestBehavior {
        delay: Duration::from_millis(delay_ms),
        cancellable: !uncancellable,
    }
}

fn parse_tool_call(body: &[u8]) -> Option<ToolCallRouteRequest> {
    let parsed = serde_json::from_slice::<ToolCallRouteRequest>(body).ok()?;
    if parsed.name.trim().is_empty() || !parsed.arguments.is_object() {
        return None;
    }
    Some(parsed)
}

fn lock_in_flight(
    in_flight: &InFlightRegistry,
) -> Result<std::sync::MutexGuard<'_, HashMap<InFlightKey, oneshot::Sender<()>>>, StubError> {
    in_flight.lock().map_err(|_| StubError::InFlightPoisoned)
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

fn manifest(
    module_id: &str,
    role: StubRole,
    concurrency: Concurrency,
    tools: &[String],
) -> subc_protocol::manifest::ModuleManifest {
    subc_protocol::manifest::ModuleManifest {
        module_id: module_id.to_string(),
        module_version: "0.0.0-fake".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![provider_role(role, concurrency, tools)],
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

fn provider_role(role: StubRole, concurrency: Concurrency, tools: &[String]) -> ProviderRole {
    match role {
        StubRole::ToolProvider => ProviderRole::ToolProvider {
            tools: tools
                .iter()
                .map(|name| Tool {
                    name: name.clone(),
                    mutates: false,
                    schema: json!({"type": "object"}),
                })
                .collect(),
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency,
            emits_push: true,
            sub_supervises: true,
        },
        StubRole::ManagementSurface => ProviderRole::ManagementSurface {
            operations: vec![
                ManagementOperation {
                    name: "memory.list".to_string(),
                    kind: ManagementOperationKind::Query,
                },
                ManagementOperation {
                    name: "bus.publish".to_string(),
                    kind: ManagementOperationKind::Mutate,
                },
            ],
            config_schema: json!({"type": "object"}),
            observability: vec![ObservabilitySurface {
                name: "fake.snapshot".to_string(),
                kind: ObservabilityKind::Snapshot,
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
        },
        StubRole::InternalService { service_id } => ProviderRole::InternalService {
            service_id,
            transport: InternalTransport::Bulk,
            agent_facing: true,
            operations: vec![
                "embed".to_string(),
                "ann_query".to_string(),
                "llm.complete".to_string(),
                "peer.forward".to_string(),
            ],
        },
        StubRole::PipelineStage => ProviderRole::PipelineStage {
            stage: PipelineStageKind::Transform,
            applies_to: PipelineAppliesTo {
                provider: "*".to_string(),
                model: "*".to_string(),
            },
            interface: "fake-pipeline-v1".to_string(),
            declares_frozen_floor: true,
            needs_signals: vec!["route.status".to_string()],
            conformance_class: "fixture".to_string(),
        },
    }
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

#[derive(Debug, Clone)]
struct StubConfig {
    connection_file_path: PathBuf,
    module_id: String,
    crash_after: Option<Duration>,
    reject_attach: bool,
    fail_registration: bool,
    events_path: Option<PathBuf>,
    emit_after_detach: bool,
    push_on_request: bool,
    fanout_on_request: bool,
    delay_from_body: bool,
    concurrency: Concurrency,
    role: StubRole,
    double_terminal: bool,
    status: Option<String>,
    toolcall_progress: bool,
    toolcall_delay: Duration,
    toolcall_result: Option<String>,
    toolcall_error: bool,
    toolcall_subc_error: bool,
    tools: Vec<String>,
}

struct StubState {
    bound_channels: BTreeSet<u16>,
    in_flight: InFlightRegistry,
}

impl Default for StubState {
    fn default() -> Self {
        Self {
            bound_channels: BTreeSet::new(),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl StubConfig {
    fn from_env() -> Result<Self, StubError> {
        let connection_file_path = parse_subc_arg(env::args_os().skip(1))?;
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
        let concurrency = concurrency_from_env()?;
        let role = role_from_env()?;
        let status = env::var(FAKE_AFT_STATUS_ENV).ok().map(|raw| {
            if raw.is_empty() {
                "idle".to_string()
            } else {
                raw
            }
        });
        let toolcall_delay = env::var(FAKE_AFT_TOOLCALL_DELAY_MS_ENV)
            .ok()
            .map(|raw| {
                raw.parse::<u64>()
                    .map(Duration::from_millis)
                    .map_err(|source| StubError::InvalidToolcallDelay { raw, source })
            })
            .transpose()?
            .unwrap_or(Duration::ZERO);
        let tools = env::var(FAKE_AFT_TOOLS_ENV)
            .ok()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|tools| !tools.is_empty())
            .unwrap_or_else(|| vec!["fake_read".to_string()]);
        let fail_registration =
            env_flag(FAKE_AFT_FAIL_REGISTRATION_ENV) || fail_registration_after_first()?;

        Ok(Self {
            connection_file_path,
            module_id,
            crash_after,
            reject_attach: env_flag(FAKE_AFT_REJECT_ATTACH_ENV),
            fail_registration,
            events_path,
            emit_after_detach: env_flag(FAKE_AFT_EMIT_AFTER_DETACH_ENV),
            push_on_request: env_flag(FAKE_AFT_PUSH_ON_REQUEST_ENV),
            fanout_on_request: env_flag(FAKE_AFT_FANOUT_ON_REQUEST_ENV),
            delay_from_body: env_flag(FAKE_AFT_DELAY_FROM_BODY_ENV),
            concurrency,
            role,
            double_terminal: env_flag(FAKE_AFT_DOUBLE_TERMINAL_ENV),
            status,
            toolcall_progress: env_flag(FAKE_AFT_TOOLCALL_PROGRESS_ENV),
            toolcall_delay,
            toolcall_result: env::var(FAKE_AFT_TOOLCALL_RESULT_ENV)
                .ok()
                .filter(|value| !value.is_empty()),
            toolcall_error: env_flag(FAKE_AFT_TOOLCALL_ERROR_ENV),
            toolcall_subc_error: env_flag(FAKE_AFT_TOOLCALL_SUBC_ERROR_ENV),
            tools,
        })
    }
}

fn parse_subc_arg(args: impl IntoIterator<Item = OsString>) -> Result<PathBuf, StubError> {
    let mut args = args.into_iter();
    let mut connection_file_path = None;
    while let Some(arg) = args.next() {
        if arg == "--subc" {
            let value = args.next().ok_or(StubError::MissingSubcValue)?;
            connection_file_path = Some(PathBuf::from(value));
            continue;
        }

        if let Some(raw) = arg.to_str().and_then(|arg| arg.strip_prefix("--subc=")) {
            if raw.is_empty() {
                return Err(StubError::MissingSubcValue);
            }
            connection_file_path = Some(PathBuf::from(raw));
            continue;
        }

        return Err(StubError::UnexpectedArg { arg });
    }

    connection_file_path.ok_or(StubError::MissingSubcArg)
}

fn concurrency_from_env() -> Result<Concurrency, StubError> {
    let Some(raw) = env::var(FAKE_AFT_CONCURRENCY_ENV).ok() else {
        return Ok(Concurrency::ModuleManaged);
    };
    match raw.as_str() {
        "serial" => Ok(Concurrency::Serial),
        "module_managed" => Ok(Concurrency::ModuleManaged),
        "stateless_parallel" => Ok(Concurrency::StatelessParallel),
        _ => Err(StubError::InvalidConcurrency { raw }),
    }
}

#[derive(Debug, Clone)]
enum StubRole {
    ToolProvider,
    ManagementSurface,
    InternalService { service_id: String },
    PipelineStage,
}

fn role_from_env() -> Result<StubRole, StubError> {
    let raw = env::var(FAKE_AFT_ROLE_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "tool_provider".to_string());
    match raw.as_str() {
        "tool_provider" => Ok(StubRole::ToolProvider),
        "management_surface" => Ok(StubRole::ManagementSurface),
        "internal_service" => Ok(StubRole::InternalService {
            service_id: env::var(FAKE_AFT_SERVICE_ID_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "internal".to_string()),
        }),
        "pipeline_stage" => Ok(StubRole::PipelineStage),
        _ => Err(StubError::InvalidRole { raw }),
    }
}

fn env_flag(key: &str) -> bool {
    matches!(
        env::var(key).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

fn fail_registration_after_first() -> Result<bool, StubError> {
    let Some(path) =
        env::var_os(FAKE_AFT_FAIL_REGISTRATION_AFTER_FIRST_PATH_ENV).map(PathBuf::from)
    else {
        return Ok(false);
    };
    if path.exists() {
        return Ok(true);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(StubError::Io)?;
    }
    fs::write(&path, b"first registration consumed\n").map_err(StubError::Io)?;
    Ok(false)
}

#[derive(Debug)]
enum StubError {
    MissingSubcArg,
    MissingSubcValue,
    UnexpectedArg {
        arg: OsString,
    },
    InvalidCrashAfter {
        raw: String,
        source: std::num::ParseIntError,
    },
    InvalidToolcallDelay {
        raw: String,
        source: std::num::ParseIntError,
    },
    InvalidConcurrency {
        raw: String,
    },
    InvalidRole {
        raw: String,
    },
    ConnectionFile {
        path: PathBuf,
        source: ConnectionFileError,
    },
    NoEndpoint {
        path: PathBuf,
    },
    InvalidEndpoint {
        path: PathBuf,
        endpoint: String,
    },
    Connect {
        path: PathBuf,
        endpoint: String,
        source: io::Error,
    },
    Auth {
        path: PathBuf,
        endpoint: String,
        source: AuthError,
    },
    Io(io::Error),
    FrameIo(subc_core::FrameIoError),
    FrameBuild(subc_core::FrameBuildError),
    Json(serde_json::Error),
    WriterClosed,
    WriterTask(tokio::task::JoinError),
    InFlightPoisoned,
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
            Self::MissingSubcArg => write!(f, "missing required --subc <connection-file-path> argument"),
            Self::MissingSubcValue => write!(f, "--subc requires a connection-file path value"),
            Self::UnexpectedArg { arg } => write!(f, "unexpected argument {:?}", arg),
            Self::InvalidCrashAfter { raw, source } => write!(
                f,
                "invalid {FAKE_AFT_CRASH_AFTER_MS_ENV} value '{raw}': {source}"
            ),
            Self::InvalidToolcallDelay { raw, source } => write!(
                f,
                "invalid {FAKE_AFT_TOOLCALL_DELAY_MS_ENV} value '{raw}': {source}"
            ),
            Self::InvalidConcurrency { raw } => write!(
                f,
                "invalid {FAKE_AFT_CONCURRENCY_ENV} value '{raw}': expected serial, module_managed, or stateless_parallel"
            ),
            Self::InvalidRole { raw } => write!(
                f,
                "invalid {FAKE_AFT_ROLE_ENV} value '{raw}': expected tool_provider, management_surface, internal_service, or pipeline_stage"
            ),
            Self::ConnectionFile { path, source } => write!(
                f,
                "failed to read subc connection file '{}': {source}",
                path.display()
            ),
            Self::NoEndpoint { path } => write!(
                f,
                "subc connection file '{}' has no endpoints",
                path.display()
            ),
            Self::InvalidEndpoint { path, endpoint } => write!(
                f,
                "subc connection file '{}' contains invalid endpoint {endpoint}",
                path.display()
            ),
            Self::Connect { path, endpoint, source } => write!(
                f,
                "failed to connect to subc endpoint {endpoint} from '{}': {source}",
                path.display()
            ),
            Self::Auth { path, endpoint, source } => write!(
                f,
                "failed to authenticate to subc endpoint {endpoint} from '{}': {source}",
                path.display()
            ),
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::FrameIo(err) => write!(f, "frame I/O error: {err}"),
            Self::FrameBuild(err) => write!(f, "frame build error: {err}"),
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::WriterClosed => write!(f, "module writer task closed"),
            Self::WriterTask(err) => write!(f, "module writer task failed: {err}"),
            Self::InFlightPoisoned => write!(f, "in-flight registry lock poisoned"),
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
            Self::InvalidToolcallDelay { source, .. } => Some(source),
            Self::ConnectionFile { source, .. } => Some(source),
            Self::Connect { source, .. } => Some(source),
            Self::Auth { source, .. } => Some(source),
            Self::Io(source) => Some(source),
            Self::FrameIo(err) => Some(err),
            Self::FrameBuild(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::WriterTask(err) => Some(err),
            Self::MissingSubcArg
            | Self::MissingSubcValue
            | Self::UnexpectedArg { .. }
            | Self::NoEndpoint { .. }
            | Self::InvalidEndpoint { .. }
            | Self::InvalidConcurrency { .. }
            | Self::InvalidRole { .. }
            | Self::WriterClosed
            | Self::InFlightPoisoned
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
