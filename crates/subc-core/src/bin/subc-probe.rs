#![forbid(unsafe_code)]

//! `subc-probe` — a thin consumer client for driving a live subc daemon by hand.
//!
//! It is the harness stand-in for end-to-end wire tests: it authenticates as a
//! client (no HELLO — that is provider-only), issues `catalog.list`, opens a
//! route to a tool provider, sends one tool-call Request on the route channel,
//! and prints the CallToolResult. Every step is logged to stderr so the real
//! subc <-> provider wire is visible.
//!
//! Usage:
//!   subc-probe --subc <connection-file> [--module-id <id>] [--root <path>]
//!              [--harness <name>] [--session <id>] [--tool <name>]
//!              [--args <json-object>] [--list-only]
//!              [--supervisor-restart <module-id>]
//!
//! Defaults: module-id = first tool_provider in the catalog; root = cwd;
//! harness = "probe"; tool = first tool of the selected provider; args = {}.

use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process,
    time::Duration,
};

use serde_json::{json, Value};
use subc_control::{CatalogEntry, ClientControlRequest, ClientControlResponse};
use subc_core::{read_frame, write_frame, Frame};
use subc_protocol::{
    manifest::ProviderRole, BindIdentity, Flags, FrameType, Priority, RouteTarget,
};
use subc_transport::{authenticate_client, connection_file};
use tokio::{net::TcpStream, time};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const USAGE: &str = "usage: subc-probe --subc <connection-file> [--module-id <id>] \
[--root <path>] [--harness <name>] [--session <id>] [--tool <name>] \
[--args <json-object>] [--list-only] [--supervisor-rescan]
             [--supervisor-restart <module-id>] [--supervisor-disable <module-id>] [--supervisor-enable <module-id>]
             [--supervisor-health] [--health-probe <module-id>]";

#[tokio::main]
async fn main() {
    if let Err(err) = run(env::args_os()).await {
        eprintln!("subc-probe: {err}");
        process::exit(1);
    }
}

struct ProbeArgs {
    connection_file: PathBuf,
    module_id: Option<String>,
    root: PathBuf,
    harness: String,
    session: String,
    tool: Option<String>,
    args: Value,
    list_only: bool,
    supervisor_rescan: bool,
    supervisor_restart: Option<String>,
    supervisor_set_enabled: Option<(String, bool)>,
    supervisor_health: bool,
    health_probe: Option<String>,
}

async fn run(argv: impl IntoIterator<Item = OsString>) -> Result<(), ProbeError> {
    let args = parse_args(argv)?;

    // 1. Discover + authenticate (client half of the HMAC handshake).
    let conn = connection_file::read_for_client(&args.connection_file).map_err(|source| {
        ProbeError::ConnectionFile {
            path: args.connection_file.clone(),
            source: source.to_string(),
        }
    })?;
    let endpoint = conn
        .endpoints
        .first()
        .ok_or_else(|| ProbeError::Message("connection file has no endpoints".into()))?;
    let ip: IpAddr = endpoint.host.parse().map_err(|_| {
        ProbeError::Message(format!("endpoint host is not an IP: {}", endpoint.host))
    })?;
    let addr = SocketAddr::new(ip, endpoint.port);
    eprintln!(
        "[probe] connecting to {addr} (daemon_ver={})",
        conn.daemon_ver
    );
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|source| ProbeError::Message(format!("connect {addr}: {source}")))?;
    authenticate_client(&mut stream, &conn, AUTH_DEADLINE)
        .await
        .map_err(|source| ProbeError::Message(format!("authenticate: {source}")))?;
    eprintln!("[probe] authenticated");

    if args.supervisor_rescan {
        let body = serde_json::to_vec(&ClientControlRequest::SupervisorRescan { preview: false })?;
        let response = control_rpc(&mut stream, body).await?;
        match response.header.ty {
            FrameType::Response => {
                let value: Value = serde_json::from_slice(&response.body)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(());
            }
            FrameType::Error => {
                return Err(ProbeError::Rejected(decode_error_body(&response.body)))
            }
            ty => {
                return Err(ProbeError::Message(format!(
                    "unexpected supervisor.rescan frame {ty:?}"
                )))
            }
        }
    }

    // Operator op: drain + respawn a single supervised module, then exit.
    // Used to roll one module onto a freshly built binary without restarting
    // the whole daemon (and without killing the process directly).
    if let Some(module_id) = &args.supervisor_restart {
        let applied = supervisor_restart(&mut stream, module_id).await?;
        eprintln!("[probe] supervisor.restart '{module_id}' -> applied={applied}");
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "module_id": module_id, "applied": applied }))?
        );
        return Ok(());
    }

    // Operator op: disable/enable a supervised module without a whole-daemon
    // restart. Disabling drains + stops the process so an offline admin tool can
    // take the module's single-writer storage lease (e.g. re-importing a stale
    // vault credential), then re-enable to respawn.
    if let Some((module_id, enabled)) = &args.supervisor_set_enabled {
        let applied = supervisor_set_enabled(&mut stream, module_id, *enabled).await?;
        eprintln!(
            "[probe] supervisor.set_enabled '{module_id}' enabled={enabled} -> applied={applied}"
        );
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({ "module_id": module_id, "enabled": enabled, "applied": applied })
            )?
        );
        return Ok(());
    }

    // Operator op: read the daemon's aggregated per-module health table (prober
    // state, last domain reports, last actions) without touching any module.
    if args.supervisor_health {
        let body = serde_json::to_vec(&ClientControlRequest::SupervisorHealth {})?;
        let response = control_rpc(&mut stream, body).await?;
        match response.header.ty {
            FrameType::Response => {
                let value: Value = serde_json::from_slice(&response.body)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(());
            }
            FrameType::Error => {
                return Err(ProbeError::Rejected(decode_error_body(&response.body)))
            }
            ty => {
                return Err(ProbeError::Message(format!(
                    "unexpected supervisor.health frame {ty:?}"
                )))
            }
        }
    }

    // Operator op: send ONE health.check to a module right now (independent of
    // the cadenced prober) and print its report — "is this module wedged?" as a
    // single command.
    if let Some(module_id) = &args.health_probe {
        let request = ClientControlRequest::SupervisorHealthProbe {
            module_id: module_id.clone(),
        };
        let response = control_rpc(&mut stream, serde_json::to_vec(&request)?).await?;
        match response.header.ty {
            FrameType::Response => {
                let value: Value = serde_json::from_slice(&response.body)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(());
            }
            FrameType::Error => {
                return Err(ProbeError::Rejected(decode_error_body(&response.body)))
            }
            ty => {
                return Err(ProbeError::Message(format!(
                    "unexpected supervisor.health_probe frame {ty:?}"
                )))
            }
        }
    }

    // 2. catalog.list — see what providers + tools are registered.
    let catalog = catalog_list(&mut stream).await?;
    eprintln!("[probe] catalog: {} provider(s)", catalog.len());
    for entry in &catalog {
        eprintln!(
            "          - {} (roles=[{}], tools=[{}])",
            entry.module_id,
            role_labels(entry).join(", "),
            tool_names(entry).join(", ")
        );
    }

    if args.list_only {
        println!("{}", serde_json::to_string_pretty(&catalog_json(&catalog))?);
        return Ok(());
    }

    // 3. Select a tool provider + a tool.
    let module_id = match &args.module_id {
        Some(id) => id.clone(),
        None => first_tool_provider(&catalog)?,
    };
    let provider = catalog
        .iter()
        .find(|e| e.module_id == module_id)
        .ok_or_else(|| ProbeError::Message(format!("module '{module_id}' not in catalog")))?;
    let tool = match &args.tool {
        Some(t) => t.clone(),
        None => tool_names(provider).into_iter().next().ok_or_else(|| {
            ProbeError::Message(format!("provider '{module_id}' exposes no tools"))
        })?,
    };

    // 4. route.open -> route_channel.
    let route = route_open(&mut stream, &module_id, &args).await?;
    eprintln!(
        "[probe] route.open '{module_id}' -> route_channel={} epoch={}",
        route.channel, route.epoch
    );

    // 5. Tool call on the route channel.
    eprintln!("[probe] tools/call '{tool}' args={}", args.args);
    let result = tool_call(&mut stream, route, &tool, &args.args).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn catalog_list(stream: &mut TcpStream) -> Result<Vec<CatalogEntry>, ProbeError> {
    let body = serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None })?;
    let response = control_rpc(stream, body).await?;
    match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
        ClientControlResponse::CatalogList { modules, .. } => Ok(modules),
        other => Err(ProbeError::Message(format!(
            "unexpected catalog.list response: {other:?}"
        ))),
    }
}

async fn supervisor_restart(stream: &mut TcpStream, module_id: &str) -> Result<bool, ProbeError> {
    let request = ClientControlRequest::SupervisorRestart {
        module_id: module_id.to_string(),
    };
    let response = control_rpc(stream, serde_json::to_vec(&request)?).await?;
    match response.header.ty {
        FrameType::Response => {
            match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
                ClientControlResponse::SupervisorAck { applied, .. } => Ok(applied),
                other => Err(ProbeError::Message(format!(
                    "unexpected supervisor.restart response: {other:?}"
                ))),
            }
        }
        FrameType::Error => Err(ProbeError::Rejected(decode_error_body(&response.body))),
        ty => Err(ProbeError::Message(format!(
            "unexpected supervisor.restart frame {ty:?}"
        ))),
    }
}

async fn supervisor_set_enabled(
    stream: &mut TcpStream,
    module_id: &str,
    enabled: bool,
) -> Result<bool, ProbeError> {
    let request = ClientControlRequest::SupervisorSetEnabled {
        module_id: module_id.to_string(),
        enabled,
    };
    let response = control_rpc(stream, serde_json::to_vec(&request)?).await?;
    match response.header.ty {
        FrameType::Response => {
            match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
                ClientControlResponse::SupervisorAck { applied, .. } => Ok(applied),
                other => Err(ProbeError::Message(format!(
                    "unexpected supervisor.set_enabled response: {other:?}"
                ))),
            }
        }
        FrameType::Error => Err(ProbeError::Rejected(decode_error_body(&response.body))),
        ty => Err(ProbeError::Message(format!(
            "unexpected supervisor.set_enabled frame {ty:?}"
        ))),
    }
}

#[derive(Clone, Copy)]
struct RouteHandle {
    channel: u16,
    epoch: u32,
}

async fn route_open(
    stream: &mut TcpStream,
    module_id: &str,
    args: &ProbeArgs,
) -> Result<RouteHandle, ProbeError> {
    let request = ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity {
            project_root: args.root.clone(),
            harness: args.harness.clone(),
            session: args.session.clone(),
        },
        consumer_identity: None,
        consumer_capabilities: None,
        admission_facts: None,
    };
    let response = control_rpc(stream, serde_json::to_vec(&request)?).await?;
    match response.header.ty {
        FrameType::Response => {
            match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
                ClientControlResponse::RouteOpen {
                    route_channel,
                    route_epoch,
                } => Ok(RouteHandle {
                    channel: route_channel,
                    epoch: route_epoch,
                }),
                other => Err(ProbeError::Message(format!(
                    "unexpected route.open response: {other:?}"
                ))),
            }
        }
        FrameType::Error => Err(ProbeError::Rejected(decode_error_body(&response.body))),
        ty => Err(ProbeError::Message(format!(
            "unexpected route.open frame {ty:?}"
        ))),
    }
}

async fn tool_call(
    stream: &mut TcpStream,
    route: RouteHandle,
    tool: &str,
    arguments: &Value,
) -> Result<Value, ProbeError> {
    let corr = 1u64;
    let body = serde_json::to_vec(&json!({ "name": tool, "arguments": arguments }))?;
    let request = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        route.channel,
        route.epoch,
        corr,
        body,
    )
    .map_err(|e| ProbeError::Message(e.to_string()))?;
    write_frame(stream, &request)
        .await
        .map_err(|e| ProbeError::Message(e.to_string()))?;

    // Read until the terminal frame for this route, surfacing interim pushes.
    loop {
        let frame = next_frame(stream).await?;
        match frame.header.ty {
            FrameType::Push => {
                eprintln!(
                    "[probe]   progress push (channel={}): {}",
                    frame.header.channel,
                    String::from_utf8_lossy(&frame.body)
                );
            }
            FrameType::Response => {
                return Ok(serde_json::from_slice(&frame.body)
                    .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&frame.body) })));
            }
            FrameType::Error => return Err(ProbeError::Rejected(decode_error_body(&frame.body))),
            ty => {
                eprintln!("[probe]   (ignoring non-terminal frame {ty:?})");
            }
        }
    }
}

/// Send a channel-0 control request and read until its channel-0 reply,
/// surfacing (and skipping) any unsolicited pushes that arrive first.
async fn control_rpc(stream: &mut TcpStream, body: Vec<u8>) -> Result<Frame, ProbeError> {
    let corr = 1u64;
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        body,
    )
    .map_err(|e| ProbeError::Message(e.to_string()))?;
    write_frame(stream, &frame)
        .await
        .map_err(|e| ProbeError::Message(e.to_string()))?;
    loop {
        let reply = next_frame(stream).await?;
        if reply.header.channel == 0
            && matches!(reply.header.ty, FrameType::Response | FrameType::Error)
        {
            return Ok(reply);
        }
        eprintln!(
            "[probe]   (skipping {:?} on channel {} while awaiting control reply)",
            reply.header.ty, reply.header.channel
        );
    }
}

async fn next_frame(stream: &mut TcpStream) -> Result<Frame, ProbeError> {
    match time::timeout(RESPONSE_TIMEOUT, read_frame(stream)).await {
        Ok(Ok(Some(frame))) => Ok(frame),
        Ok(Ok(None)) => Err(ProbeError::Message("subc closed the connection".into())),
        Ok(Err(err)) => Err(ProbeError::Message(format!("read frame: {err}"))),
        Err(_) => Err(ProbeError::Message(format!(
            "timed out after {RESPONSE_TIMEOUT:?} waiting for a frame"
        ))),
    }
}

fn decode_error_body(body: &[u8]) -> String {
    match serde_json::from_slice::<subc_protocol::ErrorBody>(body) {
        Ok(e) => format!("{} — {}", e.code, e.message),
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    }
}

fn is_tool_provider(entry: &CatalogEntry) -> bool {
    entry
        .roles
        .iter()
        .any(|r| matches!(r, ProviderRole::ToolProvider { .. }))
}

fn tool_names(entry: &CatalogEntry) -> Vec<String> {
    entry
        .roles
        .iter()
        .flat_map(|r| match r {
            ProviderRole::ToolProvider { tools, .. } => {
                tools.iter().map(|t| t.name.clone()).collect()
            }
            _ => Vec::new(),
        })
        .collect()
}

fn role_labels(entry: &CatalogEntry) -> Vec<&'static str> {
    entry
        .roles
        .iter()
        .map(|r| match r {
            ProviderRole::ToolProvider { .. } => "tool_provider",
            ProviderRole::ManagementSurface { .. } => "management_surface",
            ProviderRole::InternalService { .. } => "internal_service",
            ProviderRole::PipelineStage { .. } => "pipeline_stage",
        })
        .collect()
}

fn first_tool_provider(catalog: &[CatalogEntry]) -> Result<String, ProbeError> {
    catalog
        .iter()
        .find(|e| is_tool_provider(e))
        .map(|e| e.module_id.clone())
        .ok_or_else(|| ProbeError::Message("no tool_provider registered in catalog".into()))
}

fn catalog_json(catalog: &[CatalogEntry]) -> Value {
    Value::Array(
        catalog
            .iter()
            .map(|e| {
                json!({
                    "module_id": e.module_id,
                    "roles": role_labels(e),
                    "tools": tool_names(e),
                })
            })
            .collect(),
    )
}

fn parse_args(argv: impl IntoIterator<Item = OsString>) -> Result<ProbeArgs, ProbeError> {
    let mut args = argv.into_iter();
    let _program = args.next();

    let mut connection_file = None;
    let mut module_id = None;
    let mut root = None;
    let mut harness = String::from("probe");
    let mut session = String::from("probe-session-1");
    let mut tool = None;
    let mut args_value = json!({});
    let mut list_only = false;
    let mut supervisor_rescan = false;
    let mut supervisor_restart = None;
    let mut supervisor_set_enabled = None;
    let mut supervisor_health = false;
    let mut health_probe = None;

    while let Some(arg) = args.next() {
        if arg == OsStr::new("--subc") {
            connection_file = Some(PathBuf::from(take_value(&mut args, "--subc")?));
        } else if arg == OsStr::new("--module-id") {
            module_id = Some(take_str(&mut args, "--module-id")?);
        } else if arg == OsStr::new("--root") {
            root = Some(PathBuf::from(take_value(&mut args, "--root")?));
        } else if arg == OsStr::new("--harness") {
            harness = take_str(&mut args, "--harness")?;
        } else if arg == OsStr::new("--session") {
            session = take_str(&mut args, "--session")?;
        } else if arg == OsStr::new("--tool") {
            tool = Some(take_str(&mut args, "--tool")?);
        } else if arg == OsStr::new("--args") {
            let raw = take_str(&mut args, "--args")?;
            args_value = serde_json::from_str(&raw)
                .map_err(|e| ProbeError::Message(format!("--args is not valid JSON: {e}")))?;
        } else if arg == OsStr::new("--list-only") {
            list_only = true;
        } else if arg == OsStr::new("--supervisor-rescan") {
            supervisor_rescan = true;
        } else if arg == OsStr::new("--supervisor-restart") {
            supervisor_restart = Some(take_str(&mut args, "--supervisor-restart")?);
        } else if arg == OsStr::new("--supervisor-disable") {
            supervisor_set_enabled = Some((take_str(&mut args, "--supervisor-disable")?, false));
        } else if arg == OsStr::new("--supervisor-enable") {
            supervisor_set_enabled = Some((take_str(&mut args, "--supervisor-enable")?, true));
        } else if arg == OsStr::new("--supervisor-health") {
            supervisor_health = true;
        } else if arg == OsStr::new("--health-probe") {
            health_probe = Some(take_str(&mut args, "--health-probe")?);
        } else if arg == OsStr::new("-h") || arg == OsStr::new("--help") {
            return Err(ProbeError::Message(USAGE.into()));
        } else {
            return Err(ProbeError::Message(format!(
                "unknown argument '{}'\n{USAGE}",
                arg.to_string_lossy()
            )));
        }
    }

    let connection_file = connection_file
        .ok_or_else(|| ProbeError::Message(format!("--subc is required\n{USAGE}")))?;
    let root = match root {
        Some(r) => r,
        None => env::current_dir()
            .map_err(|e| ProbeError::Message(format!("cannot read cwd for --root: {e}")))?,
    };

    Ok(ProbeArgs {
        connection_file,
        module_id,
        root,
        harness,
        session,
        tool,
        args: args_value,
        list_only,
        supervisor_rescan,
        supervisor_restart,
        supervisor_set_enabled,
        supervisor_health,
        health_probe,
    })
}

fn take_value(
    args: &mut impl Iterator<Item = OsString>,
    flag: &str,
) -> Result<OsString, ProbeError> {
    args.next()
        .ok_or_else(|| ProbeError::Message(format!("{flag} requires a value")))
}

fn take_str(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String, ProbeError> {
    take_value(args, flag)?.into_string().map_err(|v| {
        ProbeError::Message(format!(
            "{flag} must be UTF-8, got '{}'",
            v.to_string_lossy()
        ))
    })
}

#[derive(Debug)]
enum ProbeError {
    Message(String),
    Rejected(String),
    ConnectionFile { path: PathBuf, source: String },
    Json(serde_json::Error),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(m) => write!(f, "{m}"),
            Self::Rejected(m) => write!(f, "subc rejected the request: {m}"),
            Self::ConnectionFile { path, source } => {
                write!(f, "reading connection file {}: {source}", path.display())
            }
            Self::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl Error for ProbeError {}

impl From<serde_json::Error> for ProbeError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}
