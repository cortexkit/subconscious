#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fs, io as stdio,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use subc_protocol::{
    decode_header, session::AttachAck, session::AttachRequest, session::ConfigTier, EnvelopeHeader,
    ErrorBody, Flags, FrameType, Priority, HEADER_LEN, MAX_FRAME_BODY_LEN, PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_client, authenticate_server, connection_file, generate_daemon_id, generate_key,
    ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::{self as tokio_io, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter},
    net::{tcp::OwnedReadHalf, tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::{mpsc, oneshot, Mutex},
    time,
};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const SUBC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const SHIM_SCHEMA_VERSION: u32 = 1;
const MAX_SHIM_CONTROL_MESSAGE_LEN: u32 = 64 * 1024;
const MAX_PHASE1_MESSAGE_LEN: u32 = MAX_FRAME_BODY_LEN;
const MODULE_CONNECTION_FILE_NAME: &str = "subc-mcp-connection.json";
const DEFAULT_HARNESS: &str = "mcp:generic";

const USAGE: &str = "usage:\n  subc-mcp shim [--module-connection-file <path>] [--harness <name>]\n  subc-mcp module --subc <subc-connection-file> [--connection-file <path>]";

type BoxError = Box<dyn Error + Send + Sync>;
type Result<T> = std::result::Result<T, BoxError>;
type PendingKey = (u16, u64);

#[tokio::main]
async fn main() {
    if let Err(error) = run_from_env().await {
        eprintln!("subc-mcp: {error}");
        let mut source = error.source();
        while let Some(err) = source {
            eprintln!("  caused by: {err}");
            source = err.source();
        }
        process::exit(1);
    }
}

async fn run_from_env() -> Result<()> {
    match parse_args(env::args_os())? {
        CommandMode::Shim(args) => run_shim(args).await,
        CommandMode::Module(args) => run_module(args).await,
    }
}

#[derive(Debug)]
enum CommandMode {
    Shim(ShimArgs),
    Module(ModuleArgs),
}

#[derive(Debug)]
struct ShimArgs {
    module_connection_file: Option<PathBuf>,
    harness: String,
}

#[derive(Debug)]
struct ModuleArgs {
    subc_connection_file: PathBuf,
    own_connection_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShimHello {
    schema: u32,
    project_root: PathBuf,
    harness: String,
    shim_session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShimHelloAck {
    schema: u32,
}

#[derive(Debug, Clone)]
struct EnvelopeFrame {
    header: EnvelopeHeader,
    body: Vec<u8>,
}

impl EnvelopeFrame {
    fn build(ty: FrameType, flags: Flags, channel: u16, corr: u64, body: Vec<u8>) -> Result<Self> {
        if ty.is_pure_header() && !body.is_empty() {
            return Err(other_error(format!(
                "pure-header frame {ty:?} cannot carry {} body bytes",
                body.len()
            )));
        }
        let len = u32::try_from(body.len())
            .map_err(|_| other_error(format!("frame body too large: {} bytes", body.len())))?;
        if len > MAX_FRAME_BODY_LEN {
            return Err(other_error(format!(
                "frame body too large: {len} bytes (max {MAX_FRAME_BODY_LEN})"
            )));
        }

        Ok(Self {
            header: EnvelopeHeader {
                len,
                ver: PROTOCOL_VERSION,
                ty,
                flags,
                channel,
                corr,
            },
            body,
        })
    }

    fn from_wire(header: EnvelopeHeader, body: Vec<u8>) -> Self {
        debug_assert_eq!(header.len as usize, body.len());
        Self { header, body }
    }
}

#[derive(Clone)]
struct SubcClient {
    tx: mpsc::Sender<EnvelopeFrame>,
    pending: Arc<Mutex<HashMap<PendingKey, oneshot::Sender<EnvelopeFrame>>>>,
    next_corr: Arc<AtomicU64>,
}

impl SubcClient {
    fn start(stream: TcpStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        let (tx, rx) = mpsc::channel(128);
        let pending = Arc::new(Mutex::new(HashMap::new()));

        tokio::spawn(subc_reader_loop(read_half, Arc::clone(&pending)));
        tokio::spawn(subc_writer_loop(write_half, rx));

        Self {
            tx,
            pending,
            next_corr: Arc::new(AtomicU64::new(1)),
        }
    }

    fn next_corr(&self) -> u64 {
        self.next_corr.fetch_add(1, Ordering::Relaxed)
    }

    async fn send(&self, frame: EnvelopeFrame) -> Result<()> {
        self.tx
            .send(frame)
            .await
            .map_err(|err| other_error(format!("subc writer is closed: {err}")))
    }

    async fn request(&self, frame: EnvelopeFrame, wait: Duration) -> Result<EnvelopeFrame> {
        let key = (frame.header.channel, frame.header.corr);
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.insert(key, reply_tx).is_some() {
                return Err(other_error(format!(
                    "duplicate pending subc request for channel {} corr {}",
                    key.0, key.1
                )));
            }
        }

        if let Err(err) = self.tx.send(frame).await {
            self.pending.lock().await.remove(&key);
            return Err(other_error(format!("subc writer is closed: {err}")));
        }

        match time::timeout(wait, reply_rx).await {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(_closed)) => Err(other_error(format!(
                "subc connection closed before response for channel {} corr {}",
                key.0, key.1
            ))),
            Err(_elapsed) => {
                self.pending.lock().await.remove(&key);
                Err(other_error(format!(
                    "timed out waiting {wait:?} for subc response on channel {} corr {}",
                    key.0, key.1
                )))
            }
        }
    }
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<CommandMode> {
    let mut args = args.into_iter();
    let _program = args.next();
    let Some(mode) = args.next() else {
        return Err(invalid_input(USAGE));
    };

    if mode == OsStr::new("shim") {
        parse_shim_args(args).map(CommandMode::Shim)
    } else if mode == OsStr::new("module") {
        parse_module_args(args).map(CommandMode::Module)
    } else {
        Err(invalid_input(format!(
            "unknown subcommand '{}'.\n{USAGE}",
            mode.to_string_lossy()
        )))
    }
}

fn parse_shim_args(args: impl IntoIterator<Item = OsString>) -> Result<ShimArgs> {
    let mut module_connection_file = None;
    let mut harness = DEFAULT_HARNESS.to_owned();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        if arg == OsStr::new("--module-connection-file") {
            module_connection_file = Some(PathBuf::from(take_value(
                &mut args,
                "--module-connection-file",
            )?));
        } else if arg == OsStr::new("--harness") {
            let raw = take_value(&mut args, "--harness")?;
            harness = raw.into_string().map_err(|value| {
                invalid_input(format!(
                    "--harness must be valid UTF-8, got '{}'",
                    value.to_string_lossy()
                ))
            })?;
            if harness.trim().is_empty() {
                return Err(invalid_input("--harness must not be empty"));
            }
        } else {
            return Err(invalid_input(format!(
                "unknown shim argument '{}'.\n{USAGE}",
                arg.to_string_lossy()
            )));
        }
    }

    Ok(ShimArgs {
        module_connection_file,
        harness,
    })
}

fn parse_module_args(args: impl IntoIterator<Item = OsString>) -> Result<ModuleArgs> {
    let mut subc_connection_file = None;
    let mut own_connection_file = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        if arg == OsStr::new("--subc") {
            subc_connection_file = Some(PathBuf::from(take_value(&mut args, "--subc")?));
        } else if arg == OsStr::new("--connection-file") {
            own_connection_file = Some(PathBuf::from(take_value(&mut args, "--connection-file")?));
        } else {
            return Err(invalid_input(format!(
                "unknown module argument '{}'.\n{USAGE}",
                arg.to_string_lossy()
            )));
        }
    }

    let Some(subc_connection_file) = subc_connection_file else {
        return Err(invalid_input(format!(
            "missing required module argument --subc <subc-connection-file>.\n{USAGE}"
        )));
    };

    Ok(ModuleArgs {
        subc_connection_file,
        own_connection_file,
    })
}

fn take_value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString> {
    args.next()
        .ok_or_else(|| invalid_input(format!("missing value for {flag}.\n{USAGE}")))
}

async fn run_shim(args: ShimArgs) -> Result<()> {
    let project_root = resolve_project_root()?;
    let connection_file_path = args
        .module_connection_file
        .unwrap_or_else(default_module_connection_file_path);
    let mut stream = connect_authenticated(&connection_file_path).await?;

    let hello = ShimHello {
        schema: SHIM_SCHEMA_VERSION,
        project_root,
        harness: args.harness,
        shim_session_id: generated_id("shim")?,
    };
    write_json_message(&mut stream, &hello, MAX_SHIM_CONTROL_MESSAGE_LEN).await?;

    let ack: ShimHelloAck = read_json_message(&mut stream, MAX_SHIM_CONTROL_MESSAGE_LEN).await?;
    if ack.schema != SHIM_SCHEMA_VERSION {
        return Err(other_error(format!(
            "module replied with unsupported ShimHelloAck schema {} (expected {SHIM_SCHEMA_VERSION})",
            ack.schema
        )));
    }

    pipe_stdio(stream).await
}

async fn run_module(args: ModuleArgs) -> Result<()> {
    let subc_stream = connect_authenticated(&args.subc_connection_file).await?;
    let subc = SubcClient::start(subc_stream);

    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .map_err(|source| other_error(format!("failed to bind shim listener: {source}")))?;
    let port = listener
        .local_addr()
        .map_err(|source| other_error(format!("failed to read shim listener address: {source}")))?
        .port();

    let key = generate_key()?;
    let daemon_id = generate_daemon_id()?;
    let connection_file_path = args
        .own_connection_file
        .unwrap_or_else(default_module_connection_file_path);
    publish_module_connection_file(&connection_file_path, key.clone(), daemon_id, port)?;

    loop {
        let (stream, _peer) = listener
            .accept()
            .await
            .map_err(|source| other_error(format!("failed to accept shim connection: {source}")))?;
        let subc = subc.clone();
        let key = key.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_shim_connection(stream, subc, key, daemon_id).await {
                eprintln!("subc-mcp module: shim connection failed: {error}");
            }
        });
    }
}

fn publish_module_connection_file(
    path: &Path,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
    port: u16,
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| {
            other_error(format!(
                "failed to create module connection-file directory {}: {source}",
                parent.display()
            ))
        })?;
    }

    let info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        endpoints: vec![Endpoint {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
        }],
        key,
        daemon_id,
        pid: process::id(),
        daemon_ver: env!("CARGO_PKG_VERSION").to_owned(),
    };
    connection_file::write_atomic(path, &info).map_err(|source| {
        other_error(format!(
            "failed to publish module connection file {}: {source}",
            path.display()
        ))
    })
}

async fn handle_shim_connection(
    mut stream: TcpStream,
    subc: SubcClient,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
) -> Result<()> {
    authenticate_server(
        &mut stream,
        &key,
        &daemon_id,
        env!("CARGO_PKG_VERSION"),
        AUTH_DEADLINE,
    )
    .await
    .map_err(|source| other_error(format!("shim authentication failed: {source}")))?;

    let hello: ShimHello = read_json_message(&mut stream, MAX_SHIM_CONTROL_MESSAGE_LEN).await?;
    if hello.schema != SHIM_SCHEMA_VERSION {
        return Err(other_error(format!(
            "unsupported ShimHello schema {} (expected {SHIM_SCHEMA_VERSION})",
            hello.schema
        )));
    }
    write_json_message(
        &mut stream,
        &ShimHelloAck {
            schema: SHIM_SCHEMA_VERSION,
        },
        MAX_SHIM_CONTROL_MESSAGE_LEN,
    )
    .await?;

    let route_channel = attach_session(&subc, &hello).await?;

    // PHASE 2: replace this handler with rmcp serve_server over the raw stream.
    let loop_result = phase1_length_prefixed_loop(&mut stream, &subc, route_channel).await;
    let goodbye_result = send_route_goodbye(&subc, route_channel).await;
    let _ = stream.shutdown().await;

    match (loop_result, goodbye_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(loop_error), Err(goodbye_error)) => Err(other_error(format!(
            "phase-1 shim loop failed: {loop_error}; additionally failed to send route goodbye: {goodbye_error}"
        ))),
    }
}

async fn attach_session(subc: &SubcClient, hello: &ShimHello) -> Result<u16> {
    let session = generated_session_id(&hello.shim_session_id)?;
    let attach = AttachRequest {
        project_root: hello.project_root.clone(),
        harness: hello.harness.clone(),
        session,
        config: Vec::<ConfigTier>::new(),
    };
    let body = serde_json::to_vec(&attach)?;
    let corr = subc.next_corr();
    let frame = EnvelopeFrame::build(FrameType::Request, control_flags(), 0, corr, body)?;
    let response = subc.request(frame, SUBC_RESPONSE_TIMEOUT).await?;

    match response.header.ty {
        FrameType::Response if response.header.channel == 0 => {
            let ack: AttachAck = serde_json::from_slice(&response.body)?;
            Ok(ack.route_channel)
        }
        FrameType::Error => Err(error_response(
            "subc rejected session attach",
            &response.body,
        )),
        ty => Err(other_error(format!(
            "unexpected attach response frame {ty:?} on channel {} corr {}",
            response.header.channel, response.header.corr
        ))),
    }
}

async fn phase1_length_prefixed_loop(
    stream: &mut TcpStream,
    subc: &SubcClient,
    route_channel: u16,
) -> Result<()> {
    loop {
        let Some(payload) = read_len_prefixed_bytes(stream, MAX_PHASE1_MESSAGE_LEN).await? else {
            return Ok(());
        };

        let corr = subc.next_corr();
        let frame = EnvelopeFrame::build(
            FrameType::Request,
            data_flags(),
            route_channel,
            corr,
            payload,
        )?;
        let response = subc.request(frame, SUBC_RESPONSE_TIMEOUT).await?;
        match response.header.ty {
            FrameType::Response if response.header.channel == route_channel => {
                write_len_prefixed_bytes(stream, &response.body, MAX_PHASE1_MESSAGE_LEN).await?;
                stream.flush().await?;
            }
            FrameType::Error => {
                return Err(error_response("subc route request failed", &response.body))
            }
            ty => {
                return Err(other_error(format!(
                    "unexpected route response frame {ty:?} on channel {} corr {}",
                    response.header.channel, response.header.corr
                )));
            }
        }
    }
}

async fn send_route_goodbye(subc: &SubcClient, route_channel: u16) -> Result<()> {
    let frame = EnvelopeFrame::build(
        FrameType::Goodbye,
        data_flags(),
        route_channel,
        subc.next_corr(),
        Vec::new(),
    )?;
    subc.send(frame).await
}

async fn subc_reader_loop(
    mut read_half: OwnedReadHalf,
    pending: Arc<Mutex<HashMap<PendingKey, oneshot::Sender<EnvelopeFrame>>>>,
) {
    loop {
        match read_envelope_frame(&mut read_half).await {
            Ok(Some(frame)) => {
                let key = (frame.header.channel, frame.header.corr);
                let reply = pending.lock().await.remove(&key);
                if let Some(reply) = reply {
                    let _ = reply.send(frame);
                } else {
                    eprintln!(
                        "subc-mcp module: dropping unsolicited subc frame type={:?} channel={} corr={}",
                        frame.header.ty, frame.header.channel, frame.header.corr
                    );
                }
            }
            Ok(None) => {
                eprintln!("subc-mcp module: subc connection closed");
                break;
            }
            Err(error) => {
                eprintln!("subc-mcp module: subc read failed: {error}");
                break;
            }
        }
    }

    pending.lock().await.clear();
}

async fn subc_writer_loop(mut write_half: OwnedWriteHalf, mut rx: mpsc::Receiver<EnvelopeFrame>) {
    let mut writer = BufWriter::new(&mut write_half);
    while let Some(frame) = rx.recv().await {
        if let Err(error) = write_envelope_frame(&mut writer, &frame).await {
            eprintln!("subc-mcp module: subc write failed: {error}");
            return;
        }
        while let Ok(frame) = rx.try_recv() {
            if let Err(error) = write_envelope_frame(&mut writer, &frame).await {
                eprintln!("subc-mcp module: subc write failed: {error}");
                return;
            }
        }
        if let Err(error) = writer.flush().await {
            eprintln!("subc-mcp module: subc flush failed: {error}");
            return;
        }
    }
}

async fn pipe_stdio(stream: TcpStream) -> Result<()> {
    let (mut socket_read, mut socket_write) = stream.into_split();
    let mut stdin = tokio_io::stdin();
    let mut stdout = tokio_io::stdout();

    let stdin_to_socket = async {
        let copied = tokio_io::copy(&mut stdin, &mut socket_write).await?;
        socket_write.shutdown().await?;
        stdio::Result::Ok(copied)
    };
    let socket_to_stdout = async {
        let copied = tokio_io::copy(&mut socket_read, &mut stdout).await?;
        stdout.flush().await?;
        stdio::Result::Ok(copied)
    };
    tokio::pin!(stdin_to_socket);
    tokio::pin!(socket_to_stdout);

    tokio::select! {
        result = &mut socket_to_stdout => {
            result?;
        }
        result = &mut stdin_to_socket => {
            result?;
            socket_to_stdout.await?;
        }
    }

    Ok(())
}

async fn connect_authenticated(connection_file_path: &Path) -> Result<TcpStream> {
    let conn = connection_file::read(connection_file_path).map_err(|source| {
        other_error(format!(
            "failed to read connection file {}: {source}",
            connection_file_path.display()
        ))
    })?;
    let endpoint = conn.endpoints.first().ok_or_else(|| {
        other_error(format!(
            "connection file {} has no endpoints",
            connection_file_path.display()
        ))
    })?;
    let endpoint_label = format!("{}:{}", endpoint.host, endpoint.port);
    let ip: IpAddr = endpoint.host.parse().map_err(|source| {
        other_error(format!(
            "connection file {} endpoint {endpoint_label} is not an IP address: {source}",
            connection_file_path.display()
        ))
    })?;
    let mut stream = TcpStream::connect(SocketAddr::new(ip, endpoint.port))
        .await
        .map_err(|source| {
            other_error(format!(
                "failed to connect to {} from {}: {source}",
                endpoint_label,
                connection_file_path.display()
            ))
        })?;
    authenticate_client(&mut stream, &conn, AUTH_DEADLINE)
        .await
        .map_err(|source| {
            other_error(format!(
                "failed to authenticate to {} from {}: {source}",
                endpoint_label,
                connection_file_path.display()
            ))
        })?;
    Ok(stream)
}

fn resolve_project_root() -> Result<PathBuf> {
    let mut attempted = Vec::new();
    for candidate in project_root_candidates() {
        attempted.push(candidate.display().to_string());
        if candidate.is_dir() {
            return fs::canonicalize(&candidate).map_err(|source| {
                other_error(format!(
                    "failed to canonicalize project root {}: {source}",
                    candidate.display()
                ))
            });
        }
    }

    Err(other_error(format!(
        "failed to resolve project root from CLAUDE_PROJECT_DIR, WORKSPACE_FOLDER_PATHS, or current directory; no candidate is an existing directory (attempted: {})",
        if attempted.is_empty() {
            "<none>".to_owned()
        } else {
            attempted.join(", ")
        }
    )))
}

fn project_root_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = non_empty_os_var("CLAUDE_PROJECT_DIR") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(paths) = non_empty_os_var("WORKSPACE_FOLDER_PATHS") {
        if let Some(path) = env::split_paths(&paths).next() {
            candidates.push(path);
        }
    }
    if let Ok(path) = env::current_dir() {
        candidates.push(path);
    }
    candidates
}

fn default_module_connection_file_path() -> PathBuf {
    if let Some(runtime_dir) = non_empty_os_var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join(MODULE_CONNECTION_FILE_NAME);
    }

    env::temp_dir().join(format!(
        "subc-mcp-{}.connection.json",
        user_connection_token()
    ))
}

fn user_connection_token() -> String {
    for key in ["USER", "USERNAME", "HOME", "USERPROFILE"] {
        if let Some(value) = non_empty_os_var(key) {
            return sanitize_token(&value.to_string_lossy());
        }
    }
    "unknown".to_owned()
}

fn non_empty_os_var(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
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
        "unknown".to_owned()
    } else {
        token
    }
}

fn generated_id(prefix: &str) -> Result<String> {
    Ok(format!("{prefix}-{}", hex(&generate_daemon_id()?)))
}

fn generated_session_id(shim_session_id: &str) -> Result<String> {
    Ok(format!(
        "session-{}-{}",
        sanitize_token(shim_session_id),
        hex(&generate_daemon_id()?)
    ))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}

fn data_flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}

async fn read_json_message<R, T>(reader: &mut R, max_len: u32) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let Some(bytes) = read_len_prefixed_bytes(reader, max_len).await? else {
        return Err(other_error("connection closed before JSON message"));
    };
    serde_json::from_slice(&bytes)
        .map_err(|source| other_error(format!("invalid JSON message: {source}")))
}

async fn write_json_message<W, T>(writer: &mut W, value: &T, max_len: u32) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)?;
    write_len_prefixed_bytes(writer, &bytes, max_len).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_len_prefixed_bytes<R>(reader: &mut R, max_len: u32) -> Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut len_bytes = [0u8; 4];
    if !read_exact_or_clean_eof(reader, &mut len_bytes).await? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(len_bytes);
    if len > max_len {
        return Err(other_error(format!(
            "length-prefixed message too large: {len} bytes (max {max_len})"
        )));
    }

    let mut bytes = vec![0u8; len as usize];
    if !bytes.is_empty() {
        read_exact_or_unexpected_eof(reader, &mut bytes).await?;
    }
    Ok(Some(bytes))
}

async fn write_len_prefixed_bytes<W>(writer: &mut W, bytes: &[u8], max_len: u32) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let len = u32::try_from(bytes.len())
        .map_err(|_| other_error(format!("message too large: {} bytes", bytes.len())))?;
    if len > max_len {
        return Err(other_error(format!(
            "length-prefixed message too large: {len} bytes (max {max_len})"
        )));
    }

    writer.write_all(&len.to_le_bytes()).await?;
    if !bytes.is_empty() {
        writer.write_all(bytes).await?;
    }
    Ok(())
}

async fn read_envelope_frame<R>(reader: &mut R) -> Result<Option<EnvelopeFrame>>
where
    R: AsyncRead + Unpin,
{
    let mut header_bytes = [0u8; HEADER_LEN];
    if !read_exact_or_clean_eof(reader, &mut header_bytes).await? {
        return Ok(None);
    }
    let header = decode_header(&header_bytes)
        .map_err(|source| other_error(format!("failed to decode envelope header: {source}")))?;
    if header.len > MAX_FRAME_BODY_LEN {
        return Err(other_error(format!(
            "envelope body too large: {} bytes (max {MAX_FRAME_BODY_LEN})",
            header.len
        )));
    }

    let mut body = vec![0u8; header.len as usize];
    if !body.is_empty() {
        read_exact_or_unexpected_eof(reader, &mut body).await?;
    }
    Ok(Some(EnvelopeFrame::from_wire(header, body)))
}

async fn write_envelope_frame<W>(writer: &mut W, frame: &EnvelopeFrame) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if frame.header.len as usize != frame.body.len() {
        return Err(other_error(format!(
            "frame body length mismatch: header says {}, body has {} bytes",
            frame.header.len,
            frame.body.len()
        )));
    }

    writer.write_all(&frame.header.encode()).await?;
    if !frame.body.is_empty() {
        writer.write_all(&frame.body).await?;
    }
    Ok(())
}

async fn read_exact_or_clean_eof<R>(reader: &mut R, buf: &mut [u8]) -> stdio::Result<bool>
where
    R: AsyncRead + Unpin,
{
    let mut actual = 0;
    while actual < buf.len() {
        let read = reader.read(&mut buf[actual..]).await?;
        if read == 0 {
            if actual == 0 {
                return Ok(false);
            }
            return Err(unexpected_eof(buf.len(), actual));
        }
        actual += read;
    }
    Ok(true)
}

async fn read_exact_or_unexpected_eof<R>(reader: &mut R, buf: &mut [u8]) -> stdio::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut actual = 0;
    while actual < buf.len() {
        let read = reader.read(&mut buf[actual..]).await?;
        if read == 0 {
            return Err(unexpected_eof(buf.len(), actual));
        }
        actual += read;
    }
    Ok(())
}

fn unexpected_eof(expected: usize, actual: usize) -> stdio::Error {
    stdio::Error::new(
        stdio::ErrorKind::UnexpectedEof,
        format!("expected {expected} bytes, read {actual} before EOF"),
    )
}

fn error_response(prefix: &str, body: &[u8]) -> BoxError {
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(error) => other_error(format!("{prefix}: {}: {}", error.code, error.message)),
        Err(source) => other_error(format!(
            "{prefix}: invalid error body ({} bytes): {source}",
            body.len()
        )),
    }
}

fn invalid_input(message: impl Into<String>) -> BoxError {
    stdio::Error::new(stdio::ErrorKind::InvalidInput, message.into()).into()
}

fn other_error(message: impl Into<String>) -> BoxError {
    stdio::Error::other(message.into()).into()
}
