#![forbid(unsafe_code)]

use std::{
    collections::{HashMap, HashSet},
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fs, io as stdio,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, ErrorCode, ErrorData, Implementation, JsonObject,
        ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ProgressToken,
        ServerCapabilities, ServerInfo, Tool as McpTool, ToolAnnotations,
    },
    service::{NotificationContext, Peer, RequestContext},
    transport::async_rw::AsyncRwTransport,
    RoleServer, ServerHandler,
};
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use subc_control::{CatalogEntry, ClientControlRequest, ClientControlResponse};
use subc_protocol::{
    decode_header,
    manifest::{ProviderRole, Tool as ManifestTool},
    session::ConfigTier,
    BindIdentity, EnvelopeHeader, ErrorBody, Flags, FrameType, Priority, RouteTarget, HEADER_LEN,
    MAX_FRAME_BODY_LEN, PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_client, authenticate_server, connection_file, generate_daemon_id, generate_key,
    ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::{self as tokio_io, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter},
    net::{tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::{broadcast, mpsc, watch, Mutex},
    time,
};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const SUBC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const SHIM_SCHEMA_VERSION: u32 = 1;
const MAX_SHIM_CONTROL_MESSAGE_LEN: u32 = 64 * 1024;
const MODULE_CONNECTION_FILE_NAME: &str = "subc-mcp-connection.json";
const DEFAULT_HARNESS: &str = "mcp:generic";
const PENDING_FRAME_BUFFER: usize = 8;
const SUBC_EVENT_BUFFER: usize = 64;
const CATALOG_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MCP_CONFIG_RELATIVE_PATH: &str = "cortexkit/mcp.jsonc";
const PROJECT_MCP_CONFIG_RELATIVE_PATH: &str = ".cortexkit/mcp.jsonc";

const USAGE: &str = "usage:\n  subc-mcp shim [--module-connection-file <path>] [--harness <name>]\n  subc-mcp module --subc <subc-connection-file> [--connection-file <path>]";

type BoxError = Box<dyn Error + Send + Sync>;
type Result<T> = std::result::Result<T, BoxError>;
type PendingKey = (u16, u64);
type PendingTx = mpsc::Sender<EnvelopeFrame>;

#[derive(Debug, Clone)]
enum SubcEvent {
    RouteGoodbye { route_channel: u16 },
    CatalogChanged { generation: u64 },
}

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

#[derive(Debug, Clone)]
struct AttachedSession {
    state: Arc<SessionState>,
}

#[derive(Debug, Clone)]
struct CatalogSnapshot {
    generation: u64,
    modules: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Default)]
struct GatewayConfig {
    providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Default)]
struct ProviderConfig {
    enabled: Option<bool>,
    namespace: Option<String>,
    tools: ToolConfig,
}

#[derive(Debug, Clone, Default)]
struct ToolConfig {
    default_enabled: Option<bool>,
    overrides: HashMap<String, bool>,
}

#[derive(Debug, Clone)]
struct ConfigSnapshot {
    effective: GatewayConfig,
    tiers: Vec<ConfigTier>,
}

#[derive(Debug)]
struct SessionState {
    config: ConfigSnapshot,
    identity: BindIdentity,
    inner: RwLock<SessionInner>,
}

#[derive(Debug, Clone)]
struct SessionInner {
    catalog_generation: u64,
    routes: HashMap<String, u16>,
    tools: Vec<ManifestTool>,
    bindings: HashMap<String, ToolBinding>,
}

#[derive(Debug, Clone)]
struct ToolBinding {
    module_id: String,
    route_channel: u16,
    bare_tool_name: String,
}

#[derive(Debug, Clone)]
struct DesiredSession {
    providers: Vec<DesiredProvider>,
}

#[derive(Debug, Clone)]
struct DesiredProvider {
    module_id: String,
    tools: Vec<DesiredTool>,
}

#[derive(Debug, Clone)]
struct DesiredTool {
    bare_tool: ManifestTool,
    exposed_tool: ManifestTool,
}

#[derive(Debug, Deserialize)]
struct RawGatewayConfig {
    version: u8,
    #[serde(default)]
    providers: HashMap<String, RawProviderConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RawProviderConfig {
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    enabled: MaybeSet<bool>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    namespace: MaybeSet<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    tools: MaybeSet<RawToolConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RawToolConfig {
    #[serde(
        default,
        rename = "defaultEnabled",
        deserialize_with = "deserialize_maybe_set"
    )]
    default_enabled: MaybeSet<bool>,
    #[serde(default)]
    overrides: HashMap<String, Option<bool>>,
}

#[derive(Debug, Clone, Default)]
enum MaybeSet<T> {
    #[default]
    Missing,
    Null,
    Value(T),
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
    pending: Arc<Mutex<HashMap<PendingKey, PendingTx>>>,
    events: broadcast::Sender<SubcEvent>,
    next_corr: Arc<AtomicU64>,
    catalog_poller_started: Arc<AtomicBool>,
}

impl SubcClient {
    fn start(stream: TcpStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        let (tx, rx) = mpsc::channel(128);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _events_rx) = broadcast::channel(SUBC_EVENT_BUFFER);

        tokio::spawn(subc_reader_loop(
            read_half,
            Arc::clone(&pending),
            events.clone(),
        ));
        tokio::spawn(subc_writer_loop(write_half, rx));

        Self {
            tx,
            pending,
            events,
            next_corr: Arc::new(AtomicU64::new(1)),
            catalog_poller_started: Arc::new(AtomicBool::new(false)),
        }
    }

    fn next_corr(&self) -> u64 {
        self.next_corr.fetch_add(1, Ordering::Relaxed)
    }

    fn subscribe_events(&self) -> broadcast::Receiver<SubcEvent> {
        self.events.subscribe()
    }

    fn ensure_catalog_poller(&self) {
        if self
            .catalog_poller_started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let subc = self.clone();
        tokio::spawn(async move {
            let mut last_generation = None;
            let mut interval = time::interval(CATALOG_POLL_INTERVAL);
            loop {
                interval.tick().await;
                match catalog_list(&subc).await {
                    Ok(snapshot) => {
                        if last_generation != Some(snapshot.generation) {
                            last_generation = Some(snapshot.generation);
                            let _ = subc.events.send(SubcEvent::CatalogChanged {
                                generation: snapshot.generation,
                            });
                        }
                    }
                    Err(error) => {
                        eprintln!("subc-mcp module: catalog poll failed: {error}");
                    }
                }
            }
        });
    }

    async fn send(&self, frame: EnvelopeFrame) -> Result<()> {
        self.tx
            .send(frame)
            .await
            .map_err(|err| other_error(format!("subc writer is closed: {err}")))
    }

    async fn request_frames(&self, frame: EnvelopeFrame) -> Result<mpsc::Receiver<EnvelopeFrame>> {
        let key = (frame.header.channel, frame.header.corr);
        let (reply_tx, reply_rx) = mpsc::channel(PENDING_FRAME_BUFFER);
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

        Ok(reply_rx)
    }

    async fn abandon_request(&self, channel: u16, corr: u64) {
        self.pending.lock().await.remove(&(channel, corr));
    }

    async fn request(&self, frame: EnvelopeFrame, wait: Duration) -> Result<EnvelopeFrame> {
        let key = (frame.header.channel, frame.header.corr);
        let mut reply_rx = self.request_frames(frame).await?;

        match time::timeout(wait, async {
            loop {
                let Some(frame) = reply_rx.recv().await else {
                    return Err(other_error(format!(
                        "subc connection closed before response for channel {} corr {}",
                        key.0, key.1
                    )));
                };
                if is_terminal_frame_type(frame.header.ty) {
                    return Ok(frame);
                }
            }
        })
        .await
        {
            Ok(result) => result,
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

    let attached = attach_session(&subc, &hello).await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let handler = SubcMcpServer {
        subc: subc.clone(),
        state: Arc::clone(&attached.state),
        lifecycle_started: Arc::new(AtomicBool::new(false)),
        shutdown: shutdown_rx,
    };
    let (read_half, write_half) = stream.into_split();
    let transport = AsyncRwTransport::<RoleServer, _, _>::new_server(read_half, write_half);
    let serve_result = serve_mcp_server(handler, transport).await;
    let _ = shutdown_tx.send(true);
    let goodbye_result = send_route_goodbyes(&subc, attached.state.route_channels()).await;

    match (serve_result, goodbye_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(serve_error), Err(goodbye_error)) => Err(other_error(format!(
            "rmcp shim server failed: {serve_error}; additionally failed to send route goodbyes: {goodbye_error}"
        ))),
    }
}

async fn attach_session(subc: &SubcClient, hello: &ShimHello) -> Result<AttachedSession> {
    let config = read_gateway_config(&hello.project_root)?;
    let catalog = catalog_list(subc).await?;
    let desired = desired_session_from_catalog(&config.effective, &catalog.modules)?;
    let identity = BindIdentity {
        project_root: hello.project_root.clone(),
        harness: hello.harness.clone(),
        session: generated_session_id(&hello.shim_session_id)?,
    };

    let mut routes = HashMap::new();
    for provider in &desired.providers {
        match open_provider_route(subc, &provider.module_id, &identity, &config.tiers).await {
            Ok(route_channel) => {
                routes.insert(provider.module_id.clone(), route_channel);
            }
            Err(error) => {
                let opened_routes = routes.values().copied().collect::<Vec<_>>();
                let _ = send_route_goodbyes(subc, opened_routes).await;
                return Err(error);
            }
        }
    }

    let inner = session_inner_from_desired(catalog.generation, desired, routes)?;
    Ok(AttachedSession {
        state: Arc::new(SessionState::new(config, identity, inner)),
    })
}

async fn catalog_list(subc: &SubcClient) -> Result<CatalogSnapshot> {
    let request = ClientControlRequest::CatalogList { module_id: None };
    let body = serde_json::to_vec(&request)?;
    let corr = subc.next_corr();
    let frame = EnvelopeFrame::build(FrameType::Request, control_flags(), 0, corr, body)?;
    let response = subc.request(frame, SUBC_RESPONSE_TIMEOUT).await?;

    match response.header.ty {
        FrameType::Response if response.header.channel == 0 => {
            match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
                ClientControlResponse::CatalogList {
                    generation,
                    modules,
                    ..
                } => Ok(CatalogSnapshot {
                    generation,
                    modules,
                }),
                other => Err(other_error(format!(
                    "unexpected catalog.list response body: {other:?}"
                ))),
            }
        }
        FrameType::Error => Err(error_response("subc rejected catalog.list", &response.body)),
        ty => Err(other_error(format!(
            "unexpected catalog.list response frame {ty:?} on channel {} corr {}",
            response.header.channel, response.header.corr
        ))),
    }
}

async fn open_provider_route(
    subc: &SubcClient,
    module_id: &str,
    identity: &BindIdentity,
    config: &[ConfigTier],
) -> Result<u16> {
    let request = ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_owned(),
        },
        identity: identity.clone(),
        config: config.to_vec(),
    };
    let body = serde_json::to_vec(&request)?;
    let corr = subc.next_corr();
    let frame = EnvelopeFrame::build(FrameType::Request, control_flags(), 0, corr, body)?;
    let response = subc.request(frame, SUBC_RESPONSE_TIMEOUT).await?;

    match response.header.ty {
        FrameType::Response if response.header.channel == 0 => {
            match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
                ClientControlResponse::RouteOpen { route_channel } => Ok(route_channel),
                other => Err(other_error(format!(
                    "unexpected route.open response body: {other:?}"
                ))),
            }
        }
        FrameType::Error => Err(error_response(
            &format!("subc rejected route.open for provider '{module_id}'"),
            &response.body,
        )),
        ty => Err(other_error(format!(
            "unexpected route.open response frame {ty:?} on channel {} corr {}",
            response.header.channel, response.header.corr
        ))),
    }
}

fn desired_session_from_catalog(
    config: &GatewayConfig,
    modules: &[CatalogEntry],
) -> Result<DesiredSession> {
    let mut providers = Vec::new();
    let mut exposed_names = HashMap::<String, (String, String)>::new();

    for entry in modules {
        let mut manifest_tools = Vec::new();
        for role in &entry.roles {
            if let ProviderRole::ToolProvider { tools, .. } = role {
                manifest_tools.extend(tools.iter().cloned());
            }
        }
        if manifest_tools.is_empty() || !config.provider_enabled(&entry.module_id) {
            continue;
        }

        let namespace = config.provider_namespace(&entry.module_id);
        validate_mcp_name_component("provider namespace", &namespace).map_err(|message| {
            other_error(format!(
                "provider '{}' has invalid namespace '{namespace}': {message}; set providers.{}.namespace to an MCP-safe value",
                entry.module_id, entry.module_id
            ))
        })?;

        let mut tools = Vec::new();
        for tool in manifest_tools {
            validate_mcp_name_component("tool name", &tool.name).map_err(|message| {
                other_error(format!(
                    "provider '{}' manifest has invalid tool name '{}': {message}",
                    entry.module_id, tool.name
                ))
            })?;
            if !config.tool_enabled(&entry.module_id, &tool.name) {
                continue;
            }

            let exposed_name = format!("{namespace}_{}", tool.name);
            if let Some((other_module, other_bare)) = exposed_names.insert(
                exposed_name.clone(),
                (entry.module_id.clone(), tool.name.clone()),
            ) {
                return Err(other_error(format!(
                    "MCP tool name collision for '{exposed_name}': {}.{} and {}.{}",
                    other_module, other_bare, entry.module_id, tool.name
                )));
            }

            let mut exposed_tool = tool.clone();
            exposed_tool.name = exposed_name;
            tools.push(DesiredTool {
                bare_tool: tool,
                exposed_tool,
            });
        }

        providers.push(DesiredProvider {
            module_id: entry.module_id.clone(),
            tools,
        });
    }

    Ok(DesiredSession { providers })
}

fn session_inner_from_desired(
    catalog_generation: u64,
    desired: DesiredSession,
    routes: HashMap<String, u16>,
) -> Result<SessionInner> {
    let mut tools = Vec::new();
    let mut bindings = HashMap::new();

    for provider in desired.providers {
        let route_channel = *routes.get(&provider.module_id).ok_or_else(|| {
            other_error(format!(
                "missing route channel for enabled provider '{}'",
                provider.module_id
            ))
        })?;
        for desired_tool in provider.tools {
            let exposed_name = desired_tool.exposed_tool.name.clone();
            bindings.insert(
                exposed_name.clone(),
                ToolBinding {
                    module_id: provider.module_id.clone(),
                    route_channel,
                    bare_tool_name: desired_tool.bare_tool.name,
                },
            );
            tools.push(desired_tool.exposed_tool);
        }
    }

    tools.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(SessionInner {
        catalog_generation,
        routes,
        tools,
        bindings,
    })
}

impl SessionState {
    fn new(config: ConfigSnapshot, identity: BindIdentity, inner: SessionInner) -> Self {
        Self {
            config,
            identity,
            inner: RwLock::new(inner),
        }
    }

    fn exposed_tools(&self) -> Vec<ManifestTool> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .tools
            .clone()
    }

    fn get_tool(&self, name: &str) -> Option<ManifestTool> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }

    fn binding(&self, name: &str) -> Option<ToolBinding> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bindings
            .get(name)
            .cloned()
    }

    fn route_channels(&self) -> Vec<u16> {
        let mut channels = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .routes
            .values()
            .copied()
            .collect::<Vec<_>>();
        channels.sort_unstable();
        channels.dedup();
        channels
    }

    fn catalog_generation(&self) -> u64 {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .catalog_generation
    }

    fn route_snapshot(&self) -> HashMap<String, u16> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .routes
            .clone()
    }

    fn remove_route(&self, route_channel: u16) -> bool {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let old_tools = inner.tools.clone();
        let removed_modules = inner
            .routes
            .iter()
            .filter(|(_, channel)| **channel == route_channel)
            .map(|(module_id, _)| module_id.clone())
            .collect::<HashSet<_>>();
        if removed_modules.is_empty() {
            return false;
        }

        inner
            .routes
            .retain(|module_id, _| !removed_modules.contains(module_id));
        inner
            .bindings
            .retain(|_, binding| !removed_modules.contains(&binding.module_id));
        let live_names = inner.bindings.keys().cloned().collect::<HashSet<_>>();
        inner.tools.retain(|tool| live_names.contains(&tool.name));
        old_tools != inner.tools
    }

    fn replace_inner(&self, next: SessionInner) -> bool {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = inner.tools != next.tools;
        *inner = next;
        changed
    }
}

impl GatewayConfig {
    fn provider_enabled(&self, module_id: &str) -> bool {
        self.providers
            .get(module_id)
            .and_then(|provider| provider.enabled)
            .unwrap_or(true)
    }

    fn provider_namespace(&self, module_id: &str) -> String {
        self.providers
            .get(module_id)
            .and_then(|provider| provider.namespace.clone())
            .unwrap_or_else(|| module_id.to_owned())
    }

    fn tool_enabled(&self, module_id: &str, tool_name: &str) -> bool {
        let Some(provider) = self.providers.get(module_id) else {
            return true;
        };
        provider
            .tools
            .overrides
            .get(tool_name)
            .copied()
            .unwrap_or_else(|| provider.tools.default_enabled.unwrap_or(true))
    }
}

fn validate_mcp_name_component(kind: &str, value: &str) -> std::result::Result<(), String> {
    if value.is_empty() {
        return Err(format!("{kind} must not be empty"));
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(format!(
            "{kind} must contain only ASCII letters, digits, '_' or '-'"
        ))
    }
}

fn read_gateway_config(project_root: &Path) -> Result<ConfigSnapshot> {
    let mut effective = GatewayConfig::default();
    let mut tiers = Vec::new();
    let config_files = [
        ("user", user_mcp_config_path()),
        (
            "project",
            project_root.join(PROJECT_MCP_CONFIG_RELATIVE_PATH),
        ),
    ];

    for (tier, path) in config_files {
        let doc = match fs::read_to_string(&path) {
            Ok(doc) => doc,
            Err(err) if err.kind() == stdio::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(other_error(format!(
                    "failed to read {tier} MCP config {}: {err}",
                    path.display()
                )))
            }
        };
        let raw = parse_gateway_config_doc(&doc, &path)?;
        merge_gateway_config(&mut effective, raw);
        tiers.push(ConfigTier {
            tier: tier.to_owned(),
            source: absolute_config_source(&path),
            doc,
        });
    }

    Ok(ConfigSnapshot { effective, tiers })
}

fn user_mcp_config_path() -> PathBuf {
    if let Some(config_home) = non_empty_os_var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home).join(MCP_CONFIG_RELATIVE_PATH);
    }
    if let Some(home) = non_empty_os_var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join(MCP_CONFIG_RELATIVE_PATH);
    }
    PathBuf::from(".config").join(MCP_CONFIG_RELATIVE_PATH)
}

fn absolute_config_source(path: &Path) -> String {
    if path.is_absolute() {
        path.display().to_string()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
            .to_string()
    }
}

fn parse_gateway_config_doc(doc: &str, path: &Path) -> Result<RawGatewayConfig> {
    let json = jsonc_to_json(doc).map_err(|message| {
        other_error(format!(
            "invalid JSONC in MCP config {}: {message}",
            path.display()
        ))
    })?;
    let raw: RawGatewayConfig = serde_json::from_str(&json).map_err(|source| {
        other_error(format!("invalid MCP config {}: {source}", path.display()))
    })?;
    if raw.version != 1 {
        return Err(other_error(format!(
            "invalid MCP config {}: version {} is unsupported (expected 1)",
            path.display(),
            raw.version
        )));
    }
    Ok(raw)
}

fn merge_gateway_config(effective: &mut GatewayConfig, raw: RawGatewayConfig) {
    for (module_id, raw_provider) in raw.providers {
        let provider = effective.providers.entry(module_id).or_default();
        match raw_provider.enabled {
            MaybeSet::Missing => {}
            MaybeSet::Null => provider.enabled = None,
            MaybeSet::Value(enabled) => provider.enabled = Some(enabled),
        }
        match raw_provider.namespace {
            MaybeSet::Missing => {}
            MaybeSet::Null => provider.namespace = None,
            MaybeSet::Value(namespace) => provider.namespace = Some(namespace),
        }
        match raw_provider.tools {
            MaybeSet::Missing => {}
            MaybeSet::Null => provider.tools = ToolConfig::default(),
            MaybeSet::Value(tools) => merge_tool_config(&mut provider.tools, tools),
        }
    }
}

fn merge_tool_config(effective: &mut ToolConfig, raw: RawToolConfig) {
    match raw.default_enabled {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.default_enabled = None,
        MaybeSet::Value(default_enabled) => effective.default_enabled = Some(default_enabled),
    }
    for (tool_name, override_value) in raw.overrides {
        match override_value {
            Some(enabled) => {
                effective.overrides.insert(tool_name, enabled);
            }
            None => {
                effective.overrides.remove(&tool_name);
            }
        }
    }
}

fn deserialize_maybe_set<'de, D, T>(deserializer: D) -> std::result::Result<MaybeSet<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(|value| match value {
        Some(value) => MaybeSet::Value(value),
        None => MaybeSet::Null,
    })
}

fn jsonc_to_json(doc: &str) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(doc.len());
    let mut chars = doc.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let _ = chars.next();
                let mut closed = false;
                let mut prev = '\0';
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                    }
                    if prev == '*' && next == '/' {
                        closed = true;
                        break;
                    }
                    prev = next;
                }
                if !closed {
                    return Err("unterminated block comment".to_owned());
                }
            }
            _ => out.push(ch),
        }
    }

    if in_string {
        return Err("unterminated string".to_owned());
    }

    Ok(remove_json_trailing_commas(&out))
}

fn remove_json_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == ',' {
            let mut lookahead = chars.clone();
            while matches!(lookahead.peek(), Some(next) if next.is_whitespace()) {
                let _ = lookahead.next();
            }
            if matches!(lookahead.peek(), Some('}' | ']')) {
                continue;
            }
        }

        out.push(ch);
    }

    out
}

async fn reconcile_session_from_catalog(
    subc: &SubcClient,
    state: &SessionState,
    catalog: CatalogSnapshot,
) -> Result<bool> {
    let desired = desired_session_from_catalog(&state.config.effective, &catalog.modules)?;
    let existing_routes = state.route_snapshot();
    let desired_modules = desired
        .providers
        .iter()
        .map(|provider| provider.module_id.clone())
        .collect::<HashSet<_>>();
    let removed_routes = existing_routes
        .iter()
        .filter_map(|(module_id, channel)| {
            (!desired_modules.contains(module_id)).then_some(*channel)
        })
        .collect::<Vec<_>>();

    let mut routes = HashMap::new();
    for provider in &desired.providers {
        if let Some(route_channel) = existing_routes.get(&provider.module_id) {
            routes.insert(provider.module_id.clone(), *route_channel);
            continue;
        }
        let route_channel = open_provider_route(
            subc,
            &provider.module_id,
            &state.identity,
            &state.config.tiers,
        )
        .await?;
        routes.insert(provider.module_id.clone(), route_channel);
    }

    let inner = session_inner_from_desired(catalog.generation, desired, routes)?;
    let changed = state.replace_inner(inner);
    if !removed_routes.is_empty() {
        let _ = send_route_goodbyes(subc, removed_routes).await;
    }
    Ok(changed)
}

async fn session_lifecycle(
    subc: SubcClient,
    state: Arc<SessionState>,
    mut events: broadcast::Receiver<SubcEvent>,
    peer: Peer<RoleServer>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            event = events.recv() => {
                match event {
                    Ok(SubcEvent::RouteGoodbye { route_channel }) => {
                        if state.remove_route(route_channel) && !notify_tool_list_changed(&peer).await {
                            break;
                        }
                    }
                    Ok(SubcEvent::CatalogChanged { generation }) => {
                        if generation == state.catalog_generation() {
                            continue;
                        }
                        match catalog_list(&subc).await {
                            Ok(catalog) => {
                                match reconcile_session_from_catalog(&subc, &state, catalog).await {
                                    Ok(true) => {
                                        if !notify_tool_list_changed(&peer).await {
                                            break;
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        eprintln!("subc-mcp module: keeping previous MCP tool snapshot after catalog reconciliation failed: {error}");
                                    }
                                }
                            }
                            Err(error) => {
                                eprintln!("subc-mcp module: failed to refresh catalog after generation {generation}: {error}");
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        match catalog_list(&subc).await {
                            Ok(catalog) => {
                                match reconcile_session_from_catalog(&subc, &state, catalog).await {
                                    Ok(true) => {
                                        if !notify_tool_list_changed(&peer).await {
                                            break;
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        eprintln!("subc-mcp module: keeping previous MCP tool snapshot after lagged catalog reconciliation failed: {error}");
                                    }
                                }
                            }
                            Err(error) => {
                                eprintln!("subc-mcp module: failed to refresh catalog after lagged events: {error}");
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn notify_tool_list_changed(peer: &Peer<RoleServer>) -> bool {
    match peer.notify_tool_list_changed().await {
        Ok(()) => true,
        Err(error) => {
            eprintln!("subc-mcp module: failed to notify MCP tools/list_changed: {error}");
            false
        }
    }
}

#[derive(Clone)]
struct SubcMcpServer {
    subc: SubcClient,
    state: Arc<SessionState>,
    lifecycle_started: Arc<AtomicBool>,
    shutdown: watch::Receiver<bool>,
}

/// v1 subc-mcp ↔ provider tool-call request contract carried as an opaque
/// subc route-channel `REQUEST` body. `name` is the provider's bare manifest
/// tool name and `arguments` is the exact MCP request object. `Tool.schema` in
/// the manifest is the agent-facing schema the provider accepts; the gateway
/// never translates arguments.
#[derive(Debug, Serialize)]
struct RouteToolCallRequest {
    name: String,
    arguments: JsonObject,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress_token: Option<ProgressToken>,
}

/// v1 subc-mcp ↔ provider progress contract carried as an opaque route-channel
/// `PUSH` body before the terminal response for the same correlation id.
#[derive(Debug, Deserialize)]
struct RouteToolProgress {
    progress: f64,
    #[serde(default)]
    total: Option<f64>,
    #[serde(default)]
    message: Option<String>,
}

impl ServerHandler for SubcMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("subc-mcp", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: self
                .state
                .exposed_tools()
                .iter()
                .map(mcp_tool_from_manifest)
                .collect(),
            ..Default::default()
        })
    }

    fn get_tool(&self, name: &str) -> Option<McpTool> {
        self.state
            .get_tool(name)
            .as_ref()
            .map(mcp_tool_from_manifest)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.call_tool_over_route(request, context).await
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if self
            .lifecycle_started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        self.subc.ensure_catalog_poller();
        let subc = self.subc.clone();
        let state = Arc::clone(&self.state);
        let events = self.subc.subscribe_events();
        let peer = context.peer.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(session_lifecycle(subc, state, events, peer, shutdown));
    }
}

impl SubcMcpServer {
    async fn call_tool_over_route(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let Some(binding) = self.state.binding(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool '{}'", request.name),
                None,
            ));
        };

        let route_channel = binding.route_channel;
        let progress_token = context.meta.get_progress_token();
        let body = RouteToolCallRequest {
            name: binding.bare_tool_name,
            arguments: request.arguments.unwrap_or_default(),
            progress_token: progress_token.clone(),
        };
        let body = serde_json::to_vec(&body).map_err(mcp_internal_error)?;
        let corr = self.subc.next_corr();
        let frame =
            EnvelopeFrame::build(FrameType::Request, data_flags(), route_channel, corr, body)
                .map_err(mcp_internal_error)?;
        let mut frames = self
            .subc
            .request_frames(frame)
            .await
            .map_err(mcp_internal_error)?;

        loop {
            tokio::select! {
                _ = context.ct.cancelled() => {
                    let cancel_result = self.send_route_cancel(route_channel, corr).await;
                    self.subc.abandon_request(route_channel, corr).await;
                    cancel_result.map_err(mcp_internal_error)?;
                    return Err(ErrorData::new(
                        ErrorCode::INTERNAL_ERROR,
                        "tool call cancelled by MCP client",
                        None,
                    ));
                }
                frame = frames.recv() => {
                    let Some(frame) = frame else {
                        return Err(ErrorData::internal_error(
                            format!(
                                "subc route {route_channel} closed before terminal tool response for corr {corr}",
                            ),
                            None,
                        ));
                    };

                    match frame.header.ty {
                        FrameType::Push if frame.header.channel == route_channel => {
                            forward_progress(&context, progress_token.clone(), &frame.body).await?;
                        }
                        FrameType::Response if frame.header.channel == route_channel => {
                            return serde_json::from_slice::<CallToolResult>(&frame.body).map_err(|source| {
                                ErrorData::internal_error(
                                    format!("provider returned malformed tool result: {source}"),
                                    None,
                                )
                            });
                        }
                        FrameType::Error => {
                            return Err(subc_error_to_mcp("subc route tool call failed", &frame.body));
                        }
                        ty => {
                            return Err(ErrorData::internal_error(
                                format!(
                                    "unexpected route frame {ty:?} on channel {} corr {}",
                                    frame.header.channel, frame.header.corr
                                ),
                                None,
                            ));
                        }
                    }
                }
            }
        }
    }

    async fn send_route_cancel(&self, route_channel: u16, corr: u64) -> Result<()> {
        let frame = EnvelopeFrame::build(
            FrameType::Cancel,
            data_flags(),
            route_channel,
            corr,
            Vec::new(),
        )?;
        self.subc.send(frame).await
    }
}

async fn serve_mcp_server<R, W>(
    handler: SubcMcpServer,
    transport: AsyncRwTransport<RoleServer, R, W>,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let running = rmcp::serve_server(handler, transport)
        .await
        .map_err(|source| other_error(format!("failed to initialize rmcp server: {source}")))?;
    running
        .waiting()
        .await
        .map(|_reason| ())
        .map_err(|source| other_error(format!("rmcp server task failed: {source}")))
}

fn mcp_tool_from_manifest(tool: &ManifestTool) -> McpTool {
    let description = format!("subc tool {}", tool.name);
    McpTool::new(
        tool.name.clone(),
        description,
        Arc::new(schema_value_to_object(&tool.schema)),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(!tool.mutates)
            .destructive(tool.mutates),
    )
}

fn schema_value_to_object(value: &serde_json::Value) -> JsonObject {
    match value {
        serde_json::Value::Object(object) => object.clone(),
        other => {
            let mut object = JsonObject::new();
            object.insert(
                "type".to_owned(),
                serde_json::Value::String("object".to_owned()),
            );
            object.insert("x-subc-schema".to_owned(), other.clone());
            object
        }
    }
}

async fn forward_progress(
    context: &RequestContext<RoleServer>,
    progress_token: Option<ProgressToken>,
    body: &[u8],
) -> std::result::Result<(), ErrorData> {
    let Some(progress_token) = progress_token else {
        return Ok(());
    };
    let progress = serde_json::from_slice::<RouteToolProgress>(body).map_err(|source| {
        ErrorData::internal_error(
            format!("provider returned malformed progress: {source}"),
            None,
        )
    })?;
    let mut notification = ProgressNotificationParam::new(progress_token, progress.progress);
    if let Some(total) = progress.total {
        notification = notification.with_total(total);
    }
    if let Some(message) = progress.message {
        notification = notification.with_message(message);
    }
    context
        .peer
        .notify_progress(notification)
        .await
        .map_err(mcp_internal_error)
}

fn subc_error_to_mcp(prefix: &str, body: &[u8]) -> ErrorData {
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(error) => ErrorData::internal_error(
            format!("{prefix}: {}: {}", error.code, error.message),
            Some(serde_json::json!({ "subc_code": error.code })),
        ),
        Err(source) => ErrorData::internal_error(
            format!(
                "{prefix}: invalid error body ({} bytes): {source}",
                body.len()
            ),
            None,
        ),
    }
}

fn mcp_internal_error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
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

async fn send_route_goodbyes(subc: &SubcClient, route_channels: Vec<u16>) -> Result<()> {
    let mut errors = Vec::new();
    for route_channel in route_channels {
        if let Err(error) = send_route_goodbye(subc, route_channel).await {
            errors.push(format!("channel {route_channel}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(other_error(format!(
            "failed to send route GOODBYE for {} route(s): {}",
            errors.len(),
            errors.join("; ")
        )))
    }
}

async fn subc_reader_loop<R>(
    mut read_half: R,
    pending: Arc<Mutex<HashMap<PendingKey, PendingTx>>>,
    events: broadcast::Sender<SubcEvent>,
) where
    R: AsyncRead + Unpin,
{
    loop {
        match read_envelope_frame(&mut read_half).await {
            Ok(Some(frame)) => {
                if frame.header.ty == FrameType::Push && frame.header.channel == 0 {
                    eprintln!(
                        "subc-mcp module: ignoring unrecognized channel-0 Push corr={}",
                        frame.header.corr
                    );
                    continue;
                }

                if frame.header.ty == FrameType::Goodbye && frame.header.channel != 0 {
                    fail_pending_on_route(
                        &pending,
                        frame.header.channel,
                        "subc route closed by provider GOODBYE",
                    )
                    .await;
                    let _ = events.send(SubcEvent::RouteGoodbye {
                        route_channel: frame.header.channel,
                    });
                    continue;
                }

                let key = (frame.header.channel, frame.header.corr);
                let terminal = is_terminal_frame_type(frame.header.ty);
                let reply = if terminal {
                    pending.lock().await.remove(&key)
                } else {
                    pending.lock().await.get(&key).cloned()
                };
                if let Some(reply) = reply {
                    if reply.send(frame).await.is_err() && !terminal {
                        pending.lock().await.remove(&key);
                    }
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

async fn fail_pending_on_route(
    pending: &Arc<Mutex<HashMap<PendingKey, PendingTx>>>,
    route_channel: u16,
    message: &str,
) {
    let replies = {
        let mut pending = pending.lock().await;
        let keys = pending
            .keys()
            .filter_map(|(channel, corr)| (*channel == route_channel).then_some((*channel, *corr)))
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| pending.remove(&key).map(|reply| (key, reply)))
            .collect::<Vec<_>>()
    };

    for ((channel, corr), reply) in replies {
        let body = match serde_json::to_vec(&ErrorBody {
            code: "target_unavailable".to_owned(),
            message: message.to_owned(),
        }) {
            Ok(body) => body,
            Err(error) => {
                eprintln!("subc-mcp module: failed to encode route GOODBYE error: {error}");
                Vec::new()
            }
        };
        let frame = match EnvelopeFrame::build(FrameType::Error, data_flags(), channel, corr, body)
        {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("subc-mcp module: failed to build route GOODBYE error: {error}");
                continue;
            }
        };
        let _ = reply.send(frame).await;
    }
}

fn is_terminal_frame_type(frame_type: FrameType) -> bool {
    matches!(
        frame_type,
        FrameType::Response | FrameType::Error | FrameType::StreamEnd
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn client_ignores_unknown_channel_zero_push() {
        let (mut server, client) = tokio::io::duplex(4096);
        let pending: Arc<Mutex<HashMap<PendingKey, PendingTx>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (reply_tx, mut reply_rx) = mpsc::channel(PENDING_FRAME_BUFFER);
        pending.lock().await.insert((0, 42), reply_tx);

        let reader_pending = Arc::clone(&pending);
        let reader = tokio::spawn(async move {
            let (events, _events_rx) = broadcast::channel(SUBC_EVENT_BUFFER);
            subc_reader_loop(client, reader_pending, events).await;
        });

        let push = EnvelopeFrame::build(
            FrameType::Push,
            control_flags(),
            0,
            42,
            br#"{"op":"catalog.changed","generation":2}"#.to_vec(),
        )
        .unwrap();
        write_envelope_frame(&mut server, &push).await.unwrap();
        server.flush().await.unwrap();

        time::sleep(Duration::from_millis(25)).await;
        assert!(
            pending.lock().await.contains_key(&(0, 42)),
            "unknown channel-0 Push must not satisfy a pending request"
        );

        let response = EnvelopeFrame::build(
            FrameType::Response,
            control_flags(),
            0,
            42,
            br#"{"op":"route.open","route_channel":7}"#.to_vec(),
        )
        .unwrap();
        write_envelope_frame(&mut server, &response).await.unwrap();
        server.flush().await.unwrap();

        let delivered = time::timeout(Duration::from_secs(1), reply_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered.header.ty, FrameType::Response);
        assert_eq!(delivered.header.channel, 0);
        assert_eq!(delivered.header.corr, 42);

        drop(server);
        reader.await.unwrap();
    }
}
