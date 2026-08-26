#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap},
    env,
    error::Error,
    ffi::OsString,
    fmt, fs,
    io::{self, Write as _},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{json, Value};
use subc_core::{read_frame, write_frame, Frame};
use subc_protocol::{
    manifest::{
        Bindings, CapabilityDeclarations, Concurrency, ExecutionMode, IdentityBinding,
        IdentityScope, InternalTransport, ManagementOperation, ManagementOperationKind,
        ObservabilityKind, ObservabilitySurface, PipelineAppliesTo, PipelineStageKind,
        ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    session::{
        HealthStatus, ModuleControlPush, ModuleControlRequest, ModuleControlResponse,
        MODULE_CONTROL_OP_HEALTH_CHECK,
    },
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
/// Exit 0 after the delay: exercises the supervisor's clean-exit arm, which
/// must keep the supervision command channel alive for later operator restarts.
const FAKE_AFT_CLEAN_EXIT_AFTER_MS_ENV: &str = "FAKE_AFT_CLEAN_EXIT_AFTER_MS";
const FAKE_AFT_REJECT_ATTACH_ENV: &str = "FAKE_AFT_REJECT_ATTACH";
const FAKE_AFT_BIND_NEVER_REPLY_ENV: &str = "FAKE_AFT_BIND_NEVER_REPLY";
const FAKE_AFT_BIND_NEVER_REPLY_AFTER_ENV: &str = "FAKE_AFT_BIND_NEVER_REPLY_AFTER";
const FAKE_AFT_MALFORMED_BIND_REPLY_ENV: &str = "FAKE_AFT_MALFORMED_BIND_REPLY";
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
const FAKE_AFT_USAGE_GET_FIXTURE_ENV: &str = "FAKE_AFT_USAGE_GET_FIXTURE";
const FAKE_AFT_ADVERTISE_HEALTH_ENV: &str = "FAKE_AFT_ADVERTISE_HEALTH";
const FAKE_AFT_HEALTH_NEVER_REPLY_ENV: &str = "FAKE_AFT_HEALTH_NEVER_REPLY";
const FAKE_AFT_HEALTH_NEVER_REPLY_FIRST_PATH_ENV: &str = "FAKE_AFT_HEALTH_NEVER_REPLY_FIRST_PATH";
const FAKE_AFT_HEALTH_STATUS_ENV: &str = "FAKE_AFT_HEALTH_STATUS";
const FAKE_AFT_HEALTH_DETAIL_ENV: &str = "FAKE_AFT_HEALTH_DETAIL";
const FAKE_AFT_HEALTH_METRICS_ENV: &str = "FAKE_AFT_HEALTH_METRICS";
/// Optional static capability block used only by daemon integration fixtures.
const FAKE_AFT_CAPABILITIES_ENV: &str = "FAKE_AFT_CAPABILITIES";
/// Presence (not value) is the trigger: when set, the stub writes
/// `FAKE_AFT_STDERR_LINE` (if any) to stderr and exits with this code before
/// ever touching the connection file. Lets a portable spawn stand in for a
/// freestanding `/bin/sh -c '...; exit N'` script, which never dialled subc
/// either -- a normal stub run would connect, HELLO, and register, and any
/// noise from that path would land in the very stderr ring these tests
/// assert on.
const FAKE_AFT_EXIT_CODE_ENV: &str = "FAKE_AFT_EXIT_CODE";
/// Text written to stderr before the `FAKE_AFT_EXIT_CODE` exit. Supports a
/// `{pid}` token, substituted with this process's pid, for tests that must
/// distinguish which generation across a restart produced a line.
const FAKE_AFT_STDERR_LINE_ENV: &str = "FAKE_AFT_STDERR_LINE";
/// Milliseconds the detached orphan writer (below) sleeps before writing
/// `FAKE_AFT_ORPHAN_WRITER_LINE` to stderr. Set alongside `FAKE_AFT_EXIT_CODE`
/// to reproduce a wedged pump: a child that inherits this process's stderr
/// pipe and keeps its write end open after the parent has already exited.
const FAKE_AFT_ORPHAN_WRITER_DELAY_MS_ENV: &str = "FAKE_AFT_ORPHAN_WRITER_DELAY_MS";
/// Text the detached orphan writer emits after its delay.
const FAKE_AFT_ORPHAN_WRITER_LINE_ENV: &str = "FAKE_AFT_ORPHAN_WRITER_LINE";
/// Internal marker set only on the re-exec'd orphan child, never by a test.
/// Distinguishes "I am the orphan, sleep then write" from "spawn an orphan"
/// on the same binary.
const FAKE_AFT_ORPHAN_WRITER_MODE_ENV: &str = "FAKE_AFT_ORPHAN_WRITER_MODE";
/// Id used when `FAKE_AFT_MODULE_ID` is absent.
///
/// TESTS THAT ASSERT A MODULE APPEARS IN THE CATALOG MUST CONFIGURE AN ID THAT
/// DIFFERS FROM THIS ONE, or the assertion is vacuous: a stub that never
/// received its id falls back to exactly the string being asserted, so
/// "registered under the configured id" and "registered under the default"
/// become the same observation. Every id in the suite today differs from
/// `fake-aft`, which makes those assertions real -- but that holds because of
/// how they happen to be NAMED, not because any test states it, so it can be
/// lost by a future test that picks this string.
///
/// Reported by CKE2E, who hit the live form in their own catalog registration
/// tests: where a module's compiled default equals its configured id, the
/// assertion passes whether or not the environment ever reached the process.
const DEFAULT_MODULE_ID: &str = "fake-aft";
const HELLO_CORR: u64 = 1;
const STUB_EGRESS_BUFFER: usize = 64;
const FAKE_AFT_FIXTURE_SUFFIX: &str = ".fixture.json";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FixtureSpec {
    stdout: String,
    exit_code: i32,
    sleep_ms: u64,
}

type InFlightKey = (u16, u32, u64);
type InFlightRegistry = Arc<Mutex<HashMap<InFlightKey, oneshot::Sender<()>>>>;

#[tokio::main]
async fn main() -> Result<(), StubError> {
    if let Some(fixture) = fixture_from_sidecar()? {
        return run_fixture(fixture).await;
    }

    // Checked before StubConfig::from_env(), which requires a `--subc
    // <connection-file-path>` argument neither of these paths receives: the
    // orphan re-exec is spawned by `spawn_orphan_writer` with no args at all,
    // and a caller using FAKE_AFT_EXIT_CODE as a portable stand-in for
    // `/bin/sh -c '...; exit N'` may spawn this binary the same way a raw
    // shell script would -- with no `--subc` argument, because a shell script
    // never dialled subc either. Checking first also means a connect/HELLO
    // failure can never land its own noise in the very stderr ring this knob
    // is configured to control.
    if env_flag(FAKE_AFT_ORPHAN_WRITER_MODE_ENV) {
        return run_detached_orphan_writer().await;
    }
    if let Some(exit_code) = exit_code_from_env()? {
        run_exit_only(exit_code).await?;
        unreachable!("run_exit_only always exits the process");
    }

    let config = StubConfig::from_env()?;
    run(config).await
}

fn fixture_from_sidecar() -> Result<Option<FixtureSpec>, StubError> {
    let Ok(executable) = env::current_exe() else {
        return Ok(None);
    };
    let mut sidecar = executable.into_os_string();
    sidecar.push(FAKE_AFT_FIXTURE_SUFFIX);
    match fs::read(PathBuf::from(sidecar)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(StubError::Json),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StubError::Io(error)),
    }
}

/// Runs a sidecar fixture without touching the connection file or network.
async fn run_fixture(fixture: FixtureSpec) -> Result<(), StubError> {
    if fixture.sleep_ms != 0 {
        sleep(Duration::from_millis(fixture.sleep_ms)).await;
    }
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(fixture.stdout.as_bytes())
        .map_err(StubError::Io)?;
    stdout.flush().map_err(StubError::Io)?;
    std::process::exit(fixture.exit_code);
}

/// Writes the configured stderr line (if any), optionally spawns the orphan
/// writer, then exits with `exit_code`. Never touches `--subc`, the
/// connection file, or the network.
async fn run_exit_only(exit_code: i32) -> Result<(), StubError> {
    if let Ok(line) = env::var(FAKE_AFT_STDERR_LINE_ENV) {
        if !line.is_empty() {
            eprintln!("{}", substitute_pid_token(&line));
        }
    }
    if let Some(delay) = env::var(FAKE_AFT_ORPHAN_WRITER_DELAY_MS_ENV)
        .ok()
        .map(|raw| {
            raw.parse::<u64>()
                .map(Duration::from_millis)
                .map_err(|source| StubError::InvalidOrphanWriterDelay { raw, source })
        })
        .transpose()?
    {
        let line = env::var(FAKE_AFT_ORPHAN_WRITER_LINE_ENV).ok();
        spawn_orphan_writer(delay, line)?;
    }
    std::process::exit(exit_code);
}

fn exit_code_from_env() -> Result<Option<i32>, StubError> {
    env::var(FAKE_AFT_EXIT_CODE_ENV)
        .ok()
        .map(|raw| {
            raw.parse::<i32>()
                .map_err(|source| StubError::InvalidExitCode { raw, source })
        })
        .transpose()
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

/// Entry point for the re-exec'd orphan: sleep, then write one line to
/// (inherited) stderr and exit. Never dials subc, never reads `--subc`.
async fn run_detached_orphan_writer() -> Result<(), StubError> {
    let delay = env::var(FAKE_AFT_ORPHAN_WRITER_DELAY_MS_ENV)
        .ok()
        .map(|raw| {
            raw.parse::<u64>()
                .map(Duration::from_millis)
                .map_err(|source| StubError::InvalidOrphanWriterDelay { raw, source })
        })
        .transpose()?
        .unwrap_or(Duration::ZERO);
    sleep(delay).await;
    if let Ok(line) = env::var(FAKE_AFT_ORPHAN_WRITER_LINE_ENV) {
        eprintln!("{line}");
    }
    Ok(())
}

/// Spawns a copy of this binary in orphan-writer mode with `Stdio::inherit()`
/// on stderr, so the child holds a duplicate of THIS process's stderr write
/// end -- the same handle the supervisor's stderr pump is reading from the
/// other side of. The child is spawned and dropped without a wait: dropping a
/// `std::process::Child` does not kill it, so it keeps that handle open past
/// this process's own exit, reproducing a wedged pump on both Unix and
/// Windows without a shell.
fn spawn_orphan_writer(delay: Duration, line: Option<String>) -> Result<(), StubError> {
    let exe = env::current_exe().map_err(StubError::Io)?;
    let mut command = Command::new(exe);
    command
        .env(FAKE_AFT_ORPHAN_WRITER_MODE_ENV, "1")
        .env(
            FAKE_AFT_ORPHAN_WRITER_DELAY_MS_ENV,
            delay.as_millis().to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(line) = line {
        command.env(FAKE_AFT_ORPHAN_WRITER_LINE_ENV, line);
    }
    command.spawn().map_err(StubError::Io)?;
    Ok(())
}

/// Substitutes a `{pid}` token in a configured stderr line with this
/// process's real pid, so successive restart generations are distinguishable.
fn substitute_pid_token(line: &str) -> String {
    line.replace("{pid}", &std::process::id().to_string())
}

async fn connect_to_subc(connection_file_path: &Path) -> Result<TcpStream, StubError> {
    // Any future reconnect loop must call this helper for every reconnect, so key
    // rotation is observed by re-reading the connection file each time.
    let conn = connection_file::read_for_client(connection_file_path).map_err(|source| {
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

    if let Some((exit_after, exit_code)) = config
        .crash_after
        .map(|after| (after, 2))
        .or_else(|| config.clean_exit_after.map(|after| (after, 0)))
    {
        let exit_timer = sleep(exit_after);
        tokio::pin!(exit_timer);
        loop {
            tokio::select! {
                _ = &mut exit_timer => {
                    std::process::exit(exit_code);
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
            config.capabilities.clone(),
        ),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: if config.advertise_health {
            Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()])
        } else {
            None
        },
        launch_nonce: config.launch_nonce.clone(),
    })
    .map_err(StubError::Json)?;
    let frame = Frame::build(FrameType::Hello, control_flags(), 0, 0, HELLO_CORR, body)
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

    if frame.header.channel != 0
        && state.bound_channels.get(&frame.header.channel).copied() != Some(frame.header.epoch)
        && state.tentative_channels.get(&frame.header.channel).copied() != Some(frame.header.epoch)
    {
        return Ok(true);
    }

    match frame.header.ty {
        FrameType::Ping if frame.header.channel == 0 => {
            let pong = Frame::build_with_version(
                frame.header.ver,
                FrameType::Pong,
                frame.header.flags,
                0,
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
            let fanout_channels = state
                .bound_channels
                .iter()
                .map(|(&channel, &epoch)| (channel, epoch))
                .collect::<Vec<_>>();
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
                let key = (frame.header.channel, frame.header.epoch, frame.header.corr);
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
    let key = (frame.header.channel, frame.header.epoch, frame.header.corr);
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
        // Flow-control interaction: CANCEL bypasses
        // request credits; the request credit returns only on this terminal.
        let _ = cancel_tx.send(());
        emit_cancelled_error(
            writer,
            config,
            frame.header.ver,
            frame.header.channel,
            frame.header.epoch,
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
    fanout_channels: Vec<(u16, u32)>,
    delay: Duration,
) -> Result<(), StubError> {
    send_requested_pushes(
        &writer,
        &config,
        frame.header.ver,
        frame.header.channel,
        frame.header.epoch,
        &fanout_channels,
    )
    .await?;

    if config.toolcall_progress && parse_tool_call(&frame.body).is_some() {
        emit_tool_call_progress(
            &writer,
            &config,
            frame.header.ver,
            frame.header.channel,
            frame.header.epoch,
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
    fanout_channels: Vec<(u16, u32)>,
    delay: Duration,
    in_flight: InFlightRegistry,
    cancel_rx: oneshot::Receiver<()>,
) -> Result<(), StubError> {
    let key = (frame.header.channel, frame.header.epoch, frame.header.corr);
    send_requested_pushes(
        &writer,
        &config,
        frame.header.ver,
        frame.header.channel,
        frame.header.epoch,
        &fanout_channels,
    )
    .await?;

    if config.toolcall_progress && parse_tool_call(&frame.body).is_some() {
        emit_tool_call_progress(
            &writer,
            &config,
            frame.header.ver,
            frame.header.channel,
            frame.header.epoch,
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
    request_epoch: u32,
    fanout_channels: &[(u16, u32)],
) -> Result<(), StubError> {
    if config.fanout_on_request {
        for &(channel, epoch) in fanout_channels {
            send_push(writer, version, channel, epoch).await?;
        }
    } else if config.push_on_request {
        send_push(writer, version, request_channel, request_epoch).await?;
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
            return emit_tool_call_subc_error(
                writer,
                config,
                frame.header.ver,
                channel,
                frame.header.epoch,
                corr,
            )
            .await;
        }
        tool_call_response_body(config, &tool_call)?
    } else if let Some(usage_body) = usage_get_response_body(config, &frame.body)? {
        usage_body
    } else {
        frame.body
    };
    let response = Frame::build_with_version(
        frame.header.ver,
        FrameType::Response,
        frame.header.flags,
        channel,
        frame.header.epoch,
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
    epoch: u32,
    corr: u64,
) -> Result<(), StubError> {
    let body = serde_json::to_vec(&ErrorBody {
        code: "cancelled".to_string(),
        message: "request cancelled by client".to_string(),
        detail: None,
    })
    .map_err(StubError::Json)?;
    let frame = Frame::build_with_version(
        version,
        FrameType::Error,
        Flags::new(false, Priority::Passive, false),
        channel,
        epoch,
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
            epoch,
            target,
            identity,
            principal,
            consumer_capabilities,
            admission_facts,
        } => {
            state.tentative_channels.insert(route_channel, epoch);
            record_event(
                config,
                json!({
                    "kind": "attach",
                    "route_channel": route_channel,
                    "corr": frame.header.corr,
                    "reject": config.reject_attach,
                    "target": target,
                    "identity": identity,
                    "principal": principal,
                    "consumer_capabilities": consumer_capabilities,
                    "admission_facts": admission_facts,
                }),
            )?;
            state.route_bind_count += 1;
            let bind_never_reply = config.bind_never_reply
                || config
                    .bind_never_reply_after
                    .is_some_and(|after| state.route_bind_count > after);
            if bind_never_reply {
                record_event(
                    config,
                    json!({
                        "kind": "attach_never_reply",
                        "route_channel": route_channel,
                        "corr": frame.header.corr,
                    }),
                )?;
                return Ok(());
            }

            if let Some(reply) = config.malformed_bind_reply {
                let response = Frame::build_with_version(
                    frame.header.ver,
                    reply.frame_type(),
                    control_flags(),
                    0,
                    0,
                    frame.header.corr,
                    b"{malformed route.bind reply".to_vec(),
                )
                .map_err(StubError::FrameBuild)?;
                send_outbound(writer, response).await?;
                state.tentative_channels.remove(&route_channel);
                record_event(
                    config,
                    json!({
                        "kind": "attach_malformed_reply",
                        "route_channel": route_channel,
                        "corr": frame.header.corr,
                        "reply": reply.as_str(),
                    }),
                )?;
                return Ok(());
            }

            if config.reject_attach {
                let body = serde_json::to_vec(&ErrorBody {
                    code: "config_divergence".to_string(),
                    message: "fake AFT rejected route.bind by FAKE_AFT_REJECT_ATTACH".to_string(),
                    detail: None,
                })
                .map_err(StubError::Json)?;
                let response = Frame::build_with_version(
                    frame.header.ver,
                    FrameType::Error,
                    control_flags(),
                    0,
                    0,
                    frame.header.corr,
                    body,
                )
                .map_err(StubError::FrameBuild)?;
                send_outbound(writer, response).await?;
                state.tentative_channels.remove(&route_channel);
                return Ok(());
            }

            let body = serde_json::to_vec(&ModuleControlResponse::RouteBindAck {})
                .map_err(StubError::Json)?;
            let response = Frame::build_with_version(
                frame.header.ver,
                FrameType::Response,
                control_flags(),
                0,
                0,
                frame.header.corr,
                body,
            )
            .map_err(StubError::FrameBuild)?;
            send_outbound(writer, response).await?;
            state.tentative_channels.remove(&route_channel);
            state.bound_channels.insert(route_channel, epoch);
            emit_status_update(writer, config, frame.header.ver, route_channel, epoch).await?;
        }
        ModuleControlRequest::HealthCheck {} => {
            record_event(
                config,
                json!({
                    "kind": "health_check",
                    "corr": frame.header.corr,
                }),
            )?;
            if config.health_never_reply {
                return Ok(());
            }
            let body = serde_json::to_vec(&ModuleControlResponse::HealthCheck {
                status: config.health_status,
                detail: config.health_detail.clone(),
                metrics: config.health_metrics.clone(),
            })
            .map_err(StubError::Json)?;
            let response = Frame::build_with_version(
                frame.header.ver,
                FrameType::Response,
                control_flags(),
                0,
                0,
                frame.header.corr,
                body,
            )
            .map_err(StubError::FrameBuild)?;
            send_outbound(writer, response).await?;
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
    state.tentative_channels.remove(&route_channel);

    if config.emit_after_detach {
        let stale = Frame::build_with_version(
            frame.header.ver,
            FrameType::Push,
            Flags::new(false, Priority::Passive, true),
            route_channel,
            frame.header.epoch,
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
    epoch: u32,
) -> Result<(), StubError> {
    let push = Frame::build_with_version(
        version,
        FrameType::Push,
        Flags::new(false, Priority::Passive, true),
        channel,
        epoch,
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
    epoch: u32,
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
        epoch,
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
    epoch: u32,
    corr: u64,
) -> Result<(), StubError> {
    let body = serde_json::to_vec(&ErrorBody {
        code: "target_unavailable".to_string(),
        message: "fake AFT injected subc-level tool-call failure".to_string(),
        detail: None,
    })
    .map_err(StubError::Json)?;
    let frame = Frame::build_with_version(
        version,
        FrameType::Error,
        Flags::new(false, Priority::Passive, false),
        channel,
        epoch,
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
    // Expose process facts only from this test stub so real-daemon tests can prove
    // dynamic supervision behavior without adding test fields to production APIs.
    let text = if tool_call.name == "_test.launch_nonce" {
        config.launch_nonce.clone().unwrap_or_default()
    } else if tool_call.name == "_test.pid" {
        std::process::id().to_string()
    } else {
        config.toolcall_result.clone().unwrap_or_else(|| {
            if is_error {
                format!("fake-aft tool error: {}", tool_call.name)
            } else {
                format!(
                    "fake-aft tool {} called with {}",
                    tool_call.name, tool_call.arguments
                )
            }
        })
    };
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
    route_epoch: u32,
) -> Result<(), StubError> {
    let Some(status) = config.status.as_ref() else {
        return Ok(());
    };
    let body = serde_json::to_vec(&ModuleControlPush::RouteStatus {
        route_channel,
        route_epoch,
        status: status.clone(),
    })
    .map_err(StubError::Json)?;
    let push = Frame::build_with_version(version, FrameType::Push, control_flags(), 0, 0, 0, body)
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

#[derive(Debug, Deserialize)]
struct ManagementRouteRequest {
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default, rename = "params")]
    _params: Value,
}

fn usage_get_response_body(config: &StubConfig, body: &[u8]) -> Result<Option<Vec<u8>>, StubError> {
    let Some(fixture) = config.usage_get_fixture.as_ref() else {
        return Ok(None);
    };
    let request =
        serde_json::from_slice::<ManagementRouteRequest>(body).map_err(StubError::Json)?;
    let is_usage_get = request.op.as_deref() == Some("usage.get")
        || request.method.as_deref() == Some("usage.get");
    if !is_usage_get {
        return Ok(None);
    }
    serde_json::to_vec(&json!({ "result": fixture }))
        .map(Some)
        .map_err(StubError::Json)
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
    capabilities: Option<CapabilityDeclarations>,
) -> subc_protocol::manifest::ModuleManifest {
    subc_protocol::manifest::ModuleManifest {
        module_id: module_id.to_string(),
        module_version: "0.0.0-fake".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![provider_role(role, concurrency, tools)],
        consumes: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: true,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
        capabilities,
        provenance: None,
    }
}

fn provider_role(role: StubRole, concurrency: Concurrency, tools: &[String]) -> ProviderRole {
    match role {
        StubRole::ToolProvider => ProviderRole::ToolProvider {
            tools: tools
                .iter()
                .map(|name| Tool {
                    name: name.clone(),
                    description: None,
                    execution_mode: ExecutionMode::Pure,
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
                    description: None,
                },
                ManagementOperation {
                    name: "bus.publish".to_string(),
                    kind: ManagementOperationKind::Mutate,
                    description: None,
                },
                ManagementOperation {
                    name: "usage.get".to_string(),
                    kind: ManagementOperationKind::Query,
                    description: None,
                },
            ],
            config_schema: json!({"type": "object"}),
            observability: vec![ObservabilitySurface {
                name: "fake.snapshot".to_string(),
                kind: ObservabilityKind::Snapshot,
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency,
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

#[derive(Debug, Clone, Copy)]
enum MalformedBindReply {
    Response,
    Error,
}

impl MalformedBindReply {
    fn frame_type(self) -> FrameType {
        match self {
            Self::Response => FrameType::Response,
            Self::Error => FrameType::Error,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
struct StubConfig {
    connection_file_path: PathBuf,
    module_id: String,
    crash_after: Option<Duration>,
    clean_exit_after: Option<Duration>,
    reject_attach: bool,
    bind_never_reply: bool,
    bind_never_reply_after: Option<usize>,
    malformed_bind_reply: Option<MalformedBindReply>,
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
    capabilities: Option<CapabilityDeclarations>,
    usage_get_fixture: Option<Value>,
    advertise_health: bool,
    health_never_reply: bool,
    health_status: HealthStatus,
    health_detail: Option<String>,
    health_metrics: Option<Value>,
    /// The launch nonce subc injected for spawn attestation and reserved HELLOs.
    /// A real supervised module reads this from the SUBC_LAUNCH_NONCE env var.
    launch_nonce: Option<String>,
}

struct StubState {
    bound_channels: BTreeMap<u16, u32>,
    tentative_channels: BTreeMap<u16, u32>,
    route_bind_count: usize,
    in_flight: InFlightRegistry,
}

impl Default for StubState {
    fn default() -> Self {
        Self {
            bound_channels: BTreeMap::new(),
            tentative_channels: BTreeMap::new(),
            route_bind_count: 0,
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
        let clean_exit_after = env::var(FAKE_AFT_CLEAN_EXIT_AFTER_MS_ENV)
            .ok()
            .map(|raw| {
                raw.parse::<u64>()
                    .map(Duration::from_millis)
                    .map_err(|source| StubError::InvalidCrashAfter { raw, source })
            })
            .transpose()?;
        let events_path = env::var_os(FAKE_AFT_EVENTS_PATH_ENV).map(PathBuf::from);
        let bind_never_reply_after = env::var(FAKE_AFT_BIND_NEVER_REPLY_AFTER_ENV)
            .ok()
            .map(|raw| {
                raw.parse::<usize>()
                    .map_err(|source| StubError::InvalidBindNeverReplyAfter { raw, source })
            })
            .transpose()?;
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
            clean_exit_after,
            reject_attach: env_flag(FAKE_AFT_REJECT_ATTACH_ENV),
            bind_never_reply: env_flag(FAKE_AFT_BIND_NEVER_REPLY_ENV),
            bind_never_reply_after,
            malformed_bind_reply: malformed_bind_reply_from_env()?,
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
            capabilities: env::var(FAKE_AFT_CAPABILITIES_ENV)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|raw| {
                    serde_json::from_str::<CapabilityDeclarations>(&raw).map_err(StubError::Json)
                })
                .transpose()?,
            usage_get_fixture: env::var(FAKE_AFT_USAGE_GET_FIXTURE_ENV)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|raw| serde_json::from_str::<Value>(&raw).map_err(StubError::Json))
                .transpose()?,
            advertise_health: env_flag(FAKE_AFT_ADVERTISE_HEALTH_ENV),
            health_never_reply: env_flag(FAKE_AFT_HEALTH_NEVER_REPLY_ENV)
                || health_never_reply_first_spawn()?,
            health_status: health_status_from_env()?,
            health_detail: env::var(FAKE_AFT_HEALTH_DETAIL_ENV)
                .ok()
                .filter(|value| !value.is_empty()),
            health_metrics: env::var(FAKE_AFT_HEALTH_METRICS_ENV)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|raw| serde_json::from_str::<Value>(&raw).map_err(StubError::Json))
                .transpose()?,
            launch_nonce: env::var(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
                .ok()
                .filter(|value| !value.is_empty()),
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

fn health_status_from_env() -> Result<HealthStatus, StubError> {
    match env::var(FAKE_AFT_HEALTH_STATUS_ENV)
        .ok()
        .as_deref()
        .unwrap_or("ok")
    {
        "ok" => Ok(HealthStatus::Ok),
        "degraded" => Ok(HealthStatus::Degraded),
        "failing" => Ok(HealthStatus::Failing),
        raw => Err(StubError::InvalidHealthStatus(raw.to_string())),
    }
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

fn malformed_bind_reply_from_env() -> Result<Option<MalformedBindReply>, StubError> {
    let Some(raw) = env::var(FAKE_AFT_MALFORMED_BIND_REPLY_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "response" => Ok(Some(MalformedBindReply::Response)),
        "error" => Ok(Some(MalformedBindReply::Error)),
        "0" | "false" | "off" | "none" => Ok(None),
        _ => Err(StubError::InvalidMalformedBindReply { raw }),
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

fn health_never_reply_first_spawn() -> Result<bool, StubError> {
    let Some(path) = env::var_os(FAKE_AFT_HEALTH_NEVER_REPLY_FIRST_PATH_ENV).map(PathBuf::from)
    else {
        return Ok(false);
    };
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(StubError::Io)?;
    }
    fs::write(&path, b"first health wedge consumed\n").map_err(StubError::Io)?;
    Ok(true)
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
    InvalidBindNeverReplyAfter {
        raw: String,
        source: std::num::ParseIntError,
    },
    InvalidToolcallDelay {
        raw: String,
        source: std::num::ParseIntError,
    },
    InvalidExitCode {
        raw: String,
        source: std::num::ParseIntError,
    },
    InvalidOrphanWriterDelay {
        raw: String,
        source: std::num::ParseIntError,
    },
    InvalidHealthStatus(String),
    InvalidConcurrency {
        raw: String,
    },
    InvalidRole {
        raw: String,
    },
    InvalidMalformedBindReply {
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
            Self::InvalidBindNeverReplyAfter { raw, source } => write!(
                f,
                "invalid {FAKE_AFT_BIND_NEVER_REPLY_AFTER_ENV} value '{raw}': {source}"
            ),
            Self::InvalidToolcallDelay { raw, source } => write!(
                f,
                "invalid {FAKE_AFT_TOOLCALL_DELAY_MS_ENV} value '{raw}': {source}"
            ),
            Self::InvalidExitCode { raw, source } => write!(
                f,
                "invalid {FAKE_AFT_EXIT_CODE_ENV} value '{raw}': {source}"
            ),
            Self::InvalidOrphanWriterDelay { raw, source } => write!(
                f,
                "invalid {FAKE_AFT_ORPHAN_WRITER_DELAY_MS_ENV} value '{raw}': {source}"
            ),
            Self::InvalidHealthStatus(raw) => write!(
                f,
                "invalid {FAKE_AFT_HEALTH_STATUS_ENV} value '{raw}': expected ok, degraded, or failing"
            ),
            Self::InvalidConcurrency { raw } => write!(
                f,
                "invalid {FAKE_AFT_CONCURRENCY_ENV} value '{raw}': expected serial, module_managed, or stateless_parallel"
            ),
            Self::InvalidRole { raw } => write!(
                f,
                "invalid {FAKE_AFT_ROLE_ENV} value '{raw}': expected tool_provider, management_surface, internal_service, or pipeline_stage"
            ),
            Self::InvalidMalformedBindReply { raw } => write!(
                f,
                "invalid {FAKE_AFT_MALFORMED_BIND_REPLY_ENV} value '{raw}': expected response, error, or none"
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
            Self::InvalidBindNeverReplyAfter { source, .. } => Some(source),
            Self::InvalidToolcallDelay { source, .. } => Some(source),
            Self::InvalidExitCode { source, .. } => Some(source),
            Self::InvalidOrphanWriterDelay { source, .. } => Some(source),
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
            | Self::InvalidHealthStatus(_)
            | Self::InvalidRole { .. }
            | Self::InvalidMalformedBindReply { .. }
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
