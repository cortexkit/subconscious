#![forbid(unsafe_code)]

//! `ck` — the CortexKit operator CLI.
//!
//! This binary is the founding piece of the CortexKit umbrella command. The
//! daemon/module control domain ships first, and the argument parser is shaped as
//! a small `<domain> <verb>` dispatcher so future domains such as `ck vault ...`,
//! `ck quota ...`, and `ck account ...` can be added without reshaping the CLI.

use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt, fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};
use subc_control::ClientControlRequest;
use subc_core::{read_frame, write_frame, Frame};
use subc_protocol::{Flags, FrameType, Priority};
use subc_transport::{authenticate_client, connection_file, ConnectionFileError, ConnectionInfo};
use tokio::{net::TcpStream, time};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_FILE_NAME: &str = "subc-connection.json";
const PROD_CONNECTION_RELATIVE_PATH: &[&str] =
    &[".local", "share", "cortexkit", "run", CONNECTION_FILE_NAME];
const USAGE: &str = "usage: ck [--subc <connection-file>] [--json] <command>\n\ncommands:\n  ck module list\n  ck module status <id>\n  ck module restart <id>\n  ck module stop <id>\n  ck module start <id>\n  ck health\n  ck daemon";

#[tokio::main]
async fn main() {
    match run(env::args_os()).await {
        Ok(()) => process::exit(0),
        Err(err) => {
            eprintln!("{err}");
            process::exit(err.exit_code());
        }
    }
}

async fn run(argv: impl IntoIterator<Item = OsString>) -> Result<(), CkError> {
    let args = parse_args(argv)?;
    let resolved = discover_connection_file(args.subc.as_deref())?;
    let mut client = CkClient::connect(resolved).await?;

    match args.command {
        Command::Module(ModuleCommand::List) => module_list(&mut client, args.json).await,
        Command::Module(ModuleCommand::Status { module_id }) => {
            module_status(&mut client, &module_id, args.json).await
        }
        Command::Module(ModuleCommand::Restart { module_id }) => {
            module_restart(&mut client, &module_id, args.json).await
        }
        Command::Module(ModuleCommand::Stop { module_id }) => {
            module_set_enabled(&mut client, &module_id, false, args.json).await
        }
        Command::Module(ModuleCommand::Start { module_id }) => {
            module_set_enabled(&mut client, &module_id, true, args.json).await
        }
        Command::Health => health(&mut client, args.json).await,
        Command::Daemon => daemon(&mut client, args.json).await,
    }
}

struct CkArgs {
    subc: Option<PathBuf>,
    json: bool,
    command: Command,
}

enum Command {
    Module(ModuleCommand),
    Health,
    Daemon,
}

enum ModuleCommand {
    List,
    Status { module_id: String },
    Restart { module_id: String },
    Stop { module_id: String },
    Start { module_id: String },
}

struct ResolvedConnection {
    path: PathBuf,
    info: ConnectionInfo,
}

struct CkClient {
    path: PathBuf,
    info: ConnectionInfo,
    stream: TcpStream,
    next_corr: u64,
}

impl CkClient {
    async fn connect(resolved: ResolvedConnection) -> Result<Self, CkError> {
        let endpoint = resolved
            .info
            .endpoints
            .first()
            .ok_or_else(|| CkError::Connection {
                path: resolved.path.clone(),
                source: "connection file has no endpoints".to_string(),
            })?;
        let ip: IpAddr = endpoint.host.parse().map_err(|_| CkError::Connection {
            path: resolved.path.clone(),
            source: format!("endpoint host is not an IP: {}", endpoint.host),
        })?;
        let addr = SocketAddr::new(ip, endpoint.port);
        let mut stream = match time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(source)) => {
                return Err(CkError::Connection {
                    path: resolved.path,
                    source: format!("connect {addr}: {source}"),
                })
            }
            Err(_) => {
                return Err(CkError::Connection {
                    path: resolved.path,
                    source: format!("connect {addr}: timed out after {CONNECT_TIMEOUT:?}"),
                })
            }
        };
        authenticate_client(&mut stream, &resolved.info, AUTH_DEADLINE)
            .await
            .map_err(|source| CkError::Connection {
                path: resolved.path.clone(),
                source: format!("authenticate: {source}"),
            })?;

        Ok(Self {
            path: resolved.path,
            info: resolved.info,
            stream,
            next_corr: 1,
        })
    }

    async fn rpc_value(&mut self, request: ClientControlRequest) -> Result<Value, CkError> {
        let frame = self.rpc_frame(request).await?;
        match frame.header.ty {
            FrameType::Response => Ok(serde_json::from_slice(&frame.body)?),
            FrameType::Error => Err(CkError::Rejected(decode_error_body(&frame.body))),
            ty => Err(CkError::Message(format!(
                "unexpected control response frame {ty:?}"
            ))),
        }
    }

    async fn rpc_frame(&mut self, request: ClientControlRequest) -> Result<Frame, CkError> {
        let corr = self.next_corr;
        self.next_corr = self.next_corr.saturating_add(1);
        let body = serde_json::to_vec(&request)?;
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            0,
            corr,
            body,
        )
        .map_err(|source| CkError::Message(source.to_string()))?;
        write_frame(&mut self.stream, &frame)
            .await
            .map_err(|source| CkError::Message(source.to_string()))?;

        loop {
            let reply = self.next_frame().await?;
            if reply.header.channel == 0
                && reply.header.corr == corr
                && matches!(reply.header.ty, FrameType::Response | FrameType::Error)
            {
                return Ok(reply);
            }
        }
    }

    async fn next_frame(&mut self) -> Result<Frame, CkError> {
        match time::timeout(RESPONSE_TIMEOUT, read_frame(&mut self.stream)).await {
            Ok(Ok(Some(frame))) => Ok(frame),
            Ok(Ok(None)) => Err(CkError::Message("subc closed the connection".into())),
            Ok(Err(source)) => Err(CkError::Message(format!("read frame: {source}"))),
            Err(_) => Err(CkError::Message(format!(
                "timed out after {RESPONSE_TIMEOUT:?} waiting for a frame"
            ))),
        }
    }
}

async fn module_list(client: &mut CkClient, json_output: bool) -> Result<(), CkError> {
    let value = supervisor_list(client).await?;
    if json_output {
        print_json(&value)?;
    } else {
        print_module_table(modules_array(&value));
    }
    Ok(())
}

async fn module_status(
    client: &mut CkClient,
    module_id: &str,
    json_output: bool,
) -> Result<(), CkError> {
    let list = supervisor_list(client).await?;
    let module = find_module(&list, module_id)
        .cloned()
        .ok_or_else(|| CkError::Rejected(format!("module_id '{module_id}' is not supervised")))?;
    let health = supervisor_health(client).await?;
    let health_entry = find_module(&health, module_id).cloned();

    if json_output {
        print_json(&json!({ "module": module, "health": health_entry }))?;
    } else {
        print_status_table(&module, health_entry.as_ref());
    }
    Ok(())
}

async fn module_restart(
    client: &mut CkClient,
    module_id: &str,
    json_output: bool,
) -> Result<(), CkError> {
    let ack = client
        .rpc_value(ClientControlRequest::SupervisorRestart {
            module_id: module_id.to_string(),
        })
        .await?;
    print_ack_with_state(client, module_id, ack, "restart", json_output).await
}

async fn module_set_enabled(
    client: &mut CkClient,
    module_id: &str,
    enabled: bool,
    json_output: bool,
) -> Result<(), CkError> {
    let ack = client
        .rpc_value(ClientControlRequest::SupervisorSetEnabled {
            module_id: module_id.to_string(),
            enabled,
        })
        .await?;
    let verb = if enabled { "start" } else { "stop" };
    print_ack_with_state(client, module_id, ack, verb, json_output).await
}

async fn print_ack_with_state(
    client: &mut CkClient,
    module_id: &str,
    ack: Value,
    verb: &str,
    json_output: bool,
) -> Result<(), CkError> {
    let list = supervisor_list(client).await?;
    let module = find_module(&list, module_id).cloned();
    let state = module
        .as_ref()
        .and_then(|value| value.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("-");
    let applied = ack
        .get("applied")
        .and_then(Value::as_bool)
        .ok_or_else(|| CkError::Message(format!("unexpected {verb} ack: {ack}")))?;

    if json_output {
        let mut output = ack;
        if let Some(object) = output.as_object_mut() {
            object.insert("state".to_string(), Value::String(state.to_string()));
            object.insert(
                "module".to_string(),
                module.unwrap_or_else(|| Value::Object(Default::default())),
            );
        }
        print_json(&output)?;
    } else {
        print_table(
            &["module", "applied", "state"],
            vec![vec![
                module_id.to_string(),
                applied.to_string(),
                state.to_string(),
            ]],
        );
    }
    Ok(())
}

async fn health(client: &mut CkClient, json_output: bool) -> Result<(), CkError> {
    let value = supervisor_health(client).await?;
    if json_output {
        print_json(&value)?;
    } else {
        print_health_table(modules_array(&value));
    }
    Ok(())
}

async fn daemon(client: &mut CkClient, json_output: bool) -> Result<(), CkError> {
    let connected_clients = client
        .rpc_value(ClientControlRequest::ServerDescribe {})
        .await?;
    if json_output {
        print_json(&connected_clients)?;
    } else {
        let uptime = connection_file_age(&client.path)
            .map(format_duration)
            .unwrap_or_else(|| "-".to_string());
        let protocol = display_field(&connected_clients, "protocol_ver");
        let clients = display_field(&connected_clients, "connected_clients");
        print_table(
            &[
                "daemon_ver",
                "protocol",
                "pid",
                "connected_clients",
                "uptime",
            ],
            vec![vec![
                client.info.daemon_ver.clone(),
                protocol,
                client.info.pid.to_string(),
                clients,
                uptime,
            ]],
        );
    }
    Ok(())
}

async fn supervisor_list(client: &mut CkClient) -> Result<Value, CkError> {
    client
        .rpc_value(ClientControlRequest::SupervisorList {})
        .await
}

async fn supervisor_health(client: &mut CkClient) -> Result<Value, CkError> {
    client
        .rpc_value(ClientControlRequest::SupervisorHealth {})
        .await
}

fn print_module_table(modules: &[Value]) {
    let rows = modules
        .iter()
        .map(|module| {
            vec![
                display_field(module, "module_id"),
                display_field(module, "state"),
                display_field(module, "enabled"),
                display_field(module, "live"),
                display_first_field(module, &["pid", "process_pid"]),
                display_first_field(module, &["restarts", "restart_count"]),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["id", "state", "enabled", "live", "pid", "restarts"], rows);
}

fn print_status_table(module: &Value, health: Option<&Value>) {
    let health_status = health
        .map(|entry| display_field(entry, "status"))
        .filter(|value| value != "-")
        .unwrap_or_else(|| display_field(module, "health"));
    let detail = health
        .map(|entry| display_field(entry, "detail"))
        .unwrap_or_else(|| "-".to_string());
    let metrics = health
        .and_then(|entry| entry.get("metrics"))
        .map(display_json_value)
        .unwrap_or_else(|| "-".to_string());
    let failures = health
        .map(|entry| display_field(entry, "consecutive_failures"))
        .unwrap_or_else(|| "-".to_string());
    let last_action = health
        .map(|entry| display_field(entry, "last_action"))
        .unwrap_or_else(|| "-".to_string());

    print_table(
        &[
            "id",
            "state",
            "enabled",
            "live",
            "pid",
            "restarts",
            "health",
            "failures",
            "last_action",
            "detail",
            "metrics",
        ],
        vec![vec![
            display_field(module, "module_id"),
            display_field(module, "state"),
            display_field(module, "enabled"),
            display_field(module, "live"),
            display_first_field(module, &["pid", "process_pid"]),
            display_first_field(module, &["restarts", "restart_count"]),
            health_status,
            failures,
            last_action,
            detail,
            metrics,
        ]],
    );
}

fn print_health_table(modules: &[Value]) {
    let rows = modules
        .iter()
        .map(|module| {
            vec![
                display_field(module, "module_id"),
                display_field(module, "status"),
                display_field(module, "consecutive_failures"),
                display_field(module, "last_action"),
                display_field(module, "last_action_ms"),
                display_field(module, "detail"),
                module
                    .get("metrics")
                    .map(display_json_value)
                    .unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "id",
            "status",
            "failures",
            "last_action",
            "last_action_ms",
            "detail",
            "metrics",
        ],
        rows,
    );
}

fn print_table(headers: &[&str], rows: Vec<Vec<String>>) {
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in &rows {
        for (idx, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(idx) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }

    print_row(headers.iter().copied(), &widths);
    for row in rows {
        print_row(row.iter().map(String::as_str), &widths);
    }
}

fn print_row<'a>(cells: impl IntoIterator<Item = &'a str>, widths: &[usize]) {
    let cells = cells.into_iter().collect::<Vec<_>>();
    for (idx, cell) in cells.iter().enumerate() {
        if idx > 0 {
            print!("  ");
        }
        let width = widths.get(idx).copied().unwrap_or_default();
        print!("{cell:<width$}");
    }
    println!();
}

fn print_json(value: &Value) -> Result<(), CkError> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn modules_array(value: &Value) -> &[Value] {
    value
        .get("modules")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn find_module<'a>(value: &'a Value, module_id: &str) -> Option<&'a Value> {
    modules_array(value).iter().find(|module| {
        module
            .get("module_id")
            .and_then(Value::as_str)
            .is_some_and(|id| id == module_id)
    })
}

fn display_first_field(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(*key).map(display_json_value))
        .unwrap_or_else(|| "-".to_string())
}

fn display_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .map(display_json_value)
        .unwrap_or_else(|| "-".to_string())
}

fn display_json_value(value: &Value) -> String {
    match value {
        Value::Null => "-".to_string(),
        Value::String(value) if value.is_empty() => "-".to_string(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
        }
    }
}

fn connection_file_age(path: &Path) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 60 * 60 * 24 {
        format!("{}h", secs / (60 * 60))
    } else {
        format!("{}d", secs / (60 * 60 * 24))
    }
}

fn parse_args(argv: impl IntoIterator<Item = OsString>) -> Result<CkArgs, CkError> {
    let mut args = argv.into_iter();
    let _program = args.next();
    let mut subc = None;
    let mut json = false;
    let mut positionals = Vec::new();

    while let Some(arg) = args.next() {
        if arg == OsStr::new("--subc") {
            subc = Some(PathBuf::from(take_value(&mut args, "--subc")?));
        } else if arg == OsStr::new("--json") {
            json = true;
        } else if arg == OsStr::new("-h") || arg == OsStr::new("--help") {
            return Err(CkError::Usage(USAGE.into()));
        } else if arg.to_string_lossy().starts_with("--") {
            return Err(CkError::Usage(format!(
                "unknown argument '{}'\n{USAGE}",
                arg.to_string_lossy()
            )));
        } else {
            positionals.push(arg.into_string().map_err(|value| {
                CkError::Usage(format!(
                    "command arguments must be UTF-8, got '{}'\n{USAGE}",
                    value.to_string_lossy()
                ))
            })?);
        }
    }

    let command = parse_command(&positionals)?;
    Ok(CkArgs {
        subc,
        json,
        command,
    })
}

fn parse_command(positionals: &[String]) -> Result<Command, CkError> {
    match positionals {
        [domain, verb] if domain == "module" && verb == "list" => {
            Ok(Command::Module(ModuleCommand::List))
        }
        [domain, verb, module_id] if domain == "module" && verb == "status" => {
            Ok(Command::Module(ModuleCommand::Status {
                module_id: module_id.clone(),
            }))
        }
        [domain, verb, module_id] if domain == "module" && verb == "restart" => {
            Ok(Command::Module(ModuleCommand::Restart {
                module_id: module_id.clone(),
            }))
        }
        [domain, verb, module_id] if domain == "module" && verb == "stop" => {
            Ok(Command::Module(ModuleCommand::Stop {
                module_id: module_id.clone(),
            }))
        }
        [domain, verb, module_id] if domain == "module" && verb == "start" => {
            Ok(Command::Module(ModuleCommand::Start {
                module_id: module_id.clone(),
            }))
        }
        [command] if command == "health" => Ok(Command::Health),
        [command] if command == "daemon" => Ok(Command::Daemon),
        [] => Err(CkError::Usage(format!("missing command\n{USAGE}"))),
        _ => Err(CkError::Usage(format!(
            "unknown command '{}'; expected a supported <domain> <verb>\n{USAGE}",
            positionals.join(" ")
        ))),
    }
}

fn take_value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString, CkError> {
    args.next()
        .ok_or_else(|| CkError::Usage(format!("{flag} requires a value\n{USAGE}")))
}

fn discover_connection_file(override_path: Option<&Path>) -> Result<ResolvedConnection, CkError> {
    let candidates = connection_file_candidates(override_path);
    let mut tried = Vec::new();

    for path in candidates {
        match connection_file::read(&path) {
            Ok(info) => return Ok(ResolvedConnection { path, info }),
            Err(source) => tried.push(TriedConnectionFile {
                path,
                reason: discovery_reason(&source),
            }),
        }
    }

    Err(CkError::Discovery { tried })
}

fn connection_file_candidates(override_path: Option<&Path>) -> Vec<PathBuf> {
    if let Some(path) = override_path {
        return vec![path.to_path_buf()];
    }

    let mut candidates = Vec::new();
    if let Some(runtime_dir) = non_empty_os_var("XDG_RUNTIME_DIR") {
        push_unique(
            &mut candidates,
            PathBuf::from(runtime_dir).join(CONNECTION_FILE_NAME),
        );
    }
    if let Some(home) = non_empty_os_var("HOME") {
        let mut path = PathBuf::from(home);
        for part in PROD_CONNECTION_RELATIVE_PATH {
            path.push(part);
        }
        push_unique(&mut candidates, path);
    }
    push_unique(&mut candidates, temp_fallback_connection_file_path());
    candidates
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn temp_fallback_connection_file_path() -> PathBuf {
    env::temp_dir().join(format!("subc-{}.connection.json", user_connection_token()))
}

fn user_connection_token() -> String {
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
        "unknown".to_string()
    } else {
        token
    }
}

fn discovery_reason(source: &ConnectionFileError) -> String {
    match source {
        ConnectionFileError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            "not found".to_string()
        }
        other => other.to_string(),
    }
}

fn decode_error_body(body: &[u8]) -> String {
    match serde_json::from_slice::<subc_protocol::ErrorBody>(body) {
        Ok(error) => format!("{} — {}", error.code, error.message),
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    }
}

#[derive(Debug)]
struct TriedConnectionFile {
    path: PathBuf,
    reason: String,
}

#[derive(Debug)]
enum CkError {
    Usage(String),
    Discovery { tried: Vec<TriedConnectionFile> },
    Connection { path: PathBuf, source: String },
    Rejected(String),
    Message(String),
    Json(serde_json::Error),
}

impl CkError {
    fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) | Self::Discovery { .. } => 2,
            Self::Connection { .. } => 3,
            Self::Rejected(_) | Self::Message(_) | Self::Json(_) => 1,
        }
    }
}

impl fmt::Display for CkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "{message}"),
            Self::Discovery { tried } => {
                let rendered = tried
                    .iter()
                    .map(|attempt| format!("{} ({})", attempt.path.display(), attempt.reason))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "no usable subc connection file found; tried: {rendered}")
            }
            Self::Connection { path, source } => {
                write!(
                    f,
                    "subc daemon at {} did not answer: {source}",
                    path.display()
                )
            }
            Self::Rejected(message) => write!(f, "{message}"),
            Self::Message(message) => write!(f, "{message}"),
            Self::Json(source) => write!(f, "json: {source}"),
        }
    }
}

impl Error for CkError {}

impl From<serde_json::Error> for CkError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}
