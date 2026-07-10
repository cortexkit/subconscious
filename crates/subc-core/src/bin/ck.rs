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
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use serde_json::{json, Value};
use subc_control::{CatalogEntry, ClientControlRequest, ClientControlResponse};
use subc_core::{read_frame, write_frame, Frame};
use subc_protocol::{BindIdentity, Flags, FrameType, Priority, RouteTarget};
use subc_transport::{authenticate_client, connection_file, ConnectionFileError, ConnectionInfo};
use tokio::{net::TcpStream, time};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_FILE_NAME: &str = "subc-connection.json";
const PROD_CONNECTION_RELATIVE_PATH: &[&str] =
    &[".local", "share", "cortexkit", "run", CONNECTION_FILE_NAME];
const QUOTA_MODULE_ID: &str = "ai-provider-quota";
const CK_HARNESS: &str = "ck";
const USAGE: &str = "usage: ck [--subc <connection-file>] [--json] <command>\n\ncommands:\n  ck module list\n  ck module status <id>\n  ck module restart <id>\n  ck module stop <id>\n  ck module start <id>\n  ck health\n  ck daemon\n  ck quota [<provider-id>]";

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
        Command::Quota { provider_id } => {
            quota(&mut client, provider_id.as_deref(), args.json).await
        }
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
    Quota { provider_id: Option<String> },
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

    async fn catalog_list(&mut self) -> Result<Vec<CatalogEntry>, CkError> {
        let value = self
            .rpc_value(ClientControlRequest::CatalogList { module_id: None })
            .await?;
        match serde_json::from_value::<ClientControlResponse>(value)? {
            ClientControlResponse::CatalogList { modules, .. } => Ok(modules),
            other => Err(CkError::Message(format!(
                "unexpected catalog.list response: {other:?}"
            ))),
        }
    }

    async fn route_open_management(
        &mut self,
        module_id: &str,
        project_root: PathBuf,
    ) -> Result<u16, CkError> {
        let request = ClientControlRequest::RouteOpen {
            target: RouteTarget::ManagementSurface {
                module_id: module_id.to_string(),
            },
            identity: BindIdentity {
                project_root,
                harness: CK_HARNESS.to_string(),
                session: "quota".to_string(),
            },
            consumer_identity: None,
            consumer_capabilities: None,
        };
        let value = self.rpc_value(request).await?;
        match serde_json::from_value::<ClientControlResponse>(value)? {
            ClientControlResponse::RouteOpen { route_channel } => Ok(route_channel),
            other => Err(CkError::Message(format!(
                "unexpected route.open response: {other:?}"
            ))),
        }
    }

    async fn route_request_value(
        &mut self,
        route_channel: u16,
        body: Value,
    ) -> Result<Value, CkError> {
        let corr = self.next_corr;
        self.next_corr = self.next_corr.saturating_add(1);
        let body = serde_json::to_vec(&body)?;
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            route_channel,
            corr,
            body,
        )
        .map_err(|source| CkError::Message(source.to_string()))?;
        write_frame(&mut self.stream, &frame)
            .await
            .map_err(|source| CkError::Message(source.to_string()))?;

        loop {
            let reply = self.next_frame().await?;
            if reply.header.channel != route_channel || reply.header.corr != corr {
                continue;
            }
            return match reply.header.ty {
                FrameType::Response => Ok(serde_json::from_slice(&reply.body)?),
                FrameType::Error => Err(CkError::Rejected(decode_error_body(&reply.body))),
                ty => Err(CkError::Message(format!(
                    "unexpected route response frame {ty:?}"
                ))),
            };
        }
    }

    async fn route_goodbye(&mut self, route_channel: u16) {
        let frame = match Frame::build(
            FrameType::Goodbye,
            Flags::new(false, Priority::Passive, false),
            route_channel,
            0,
            Vec::new(),
        ) {
            Ok(frame) => frame,
            Err(_) => return,
        };
        let _ = write_frame(&mut self.stream, &frame).await;
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

async fn quota(
    client: &mut CkClient,
    provider_filter: Option<&str>,
    json_output: bool,
) -> Result<(), CkError> {
    ensure_quota_module_registered(client).await?;
    let project_root = env::current_dir()
        .map_err(|source| CkError::Message(format!("current directory: {source}")))?;
    let route_channel = client
        .route_open_management(QUOTA_MODULE_ID, project_root)
        .await?;
    let body = client
        .route_request_value(
            route_channel,
            json!({ "method": "usage.get", "params": {} }),
        )
        .await?;
    client.route_goodbye(route_channel).await;

    let providers = usage_providers_from_body(&body)?;
    if let Some(filter) = provider_filter {
        if !providers.iter().any(|p| provider_id(p) == filter) {
            let ids = provider_ids_sorted(&providers);
            return Err(CkError::Rejected(format!(
                "unknown provider '{filter}'; valid ids: {}",
                ids.join(", ")
            )));
        }
    }

    if json_output {
        print_json(&body)?;
    } else {
        print_quota_table(&providers, provider_filter);
    }
    Ok(())
}

async fn ensure_quota_module_registered(client: &mut CkClient) -> Result<(), CkError> {
    let catalog = client.catalog_list().await?;
    if catalog
        .iter()
        .any(|entry| entry.module_id == QUOTA_MODULE_ID)
    {
        return Ok(());
    }
    Err(CkError::Rejected(format!(
        "module '{QUOTA_MODULE_ID}' is not registered — is it enabled in subc.jsonc?"
    )))
}

fn usage_providers_from_body(body: &Value) -> Result<Vec<Value>, CkError> {
    body.get("result")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| CkError::Message(format!("unexpected usage.get reply: {body}")))
}

fn provider_id(provider: &Value) -> String {
    provider
        .get("provider")
        .or_else(|| provider.get("provider_id"))
        .or_else(|| provider.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string()
}

fn provider_ids_sorted(providers: &[Value]) -> Vec<String> {
    let mut ids = providers.iter().map(provider_id).collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids
}

fn account_label(entry: &Value) -> String {
    entry
        .get("account")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

fn entry_error_detail(entry: &Value) -> Option<String> {
    entry
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn print_quota_table(providers: &[Value], filter: Option<&str>) {
    let mut rows = Vec::new();
    let mut sorted = providers.to_vec();
    sorted.sort_by_key(provider_id);

    for entry in &sorted {
        let id = provider_id(entry);
        if filter.is_some_and(|wanted| wanted != id) {
            continue;
        }
        let account = account_label(entry);
        let error_detail = entry_error_detail(entry);
        let window_rows = quota_window_rows_for_entry(entry);

        if window_rows.is_empty() {
            let detail = error_detail
                .as_deref()
                .map(truncate_cell)
                .unwrap_or_else(|| "-".to_string());
            rows.push(vec![
                id,
                account,
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
                detail,
            ]);
            continue;
        }

        for (idx, (label, window)) in window_rows.iter().enumerate() {
            let used = format!("{:>6}", format_used_percent_rate_window(window));
            let resets = format_resets_at_rate_window(window);
            let status_cell = if idx == 0 {
                error_detail
                    .as_deref()
                    .map(truncate_cell)
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let provider_cell = if idx == 0 { id.clone() } else { String::new() };
            let account_cell = if idx == 0 {
                account.clone()
            } else {
                String::new()
            };
            rows.push(vec![
                provider_cell,
                account_cell,
                label.clone(),
                used,
                resets,
                status_cell,
            ]);
        }
    }

    print_table(
        &[
            "provider",
            "account",
            "window",
            "used%",
            "resets",
            "status/detail",
        ],
        rows,
    );
}

fn quota_window_rows_for_entry(entry: &Value) -> Vec<(String, Value)> {
    let mut rows = Vec::new();
    let usage = entry.get("usage").and_then(Value::as_object);
    let Some(usage) = usage else {
        return rows;
    };

    for slot in ["primary", "secondary", "tertiary"] {
        if let Some(window) = usage.get(slot).filter(|w| !w.is_null()) {
            rows.push((rate_window_label(window, slot), window.clone()));
        }
    }

    if let Some(extras) = usage.get("extraRateWindows").and_then(Value::as_array) {
        for extra in extras {
            let label = extra_window_label(extra);
            if let Some(window) = extra.get("window").filter(|w| !w.is_null()) {
                rows.push((label, window.clone()));
            } else {
                rows.push((label, Value::Null));
            }
        }
    }

    rows.into_iter()
        .map(|(label, window)| {
            if window.is_null() {
                (label, json!({}))
            } else {
                (label, window)
            }
        })
        .collect()
}

fn extra_window_label(extra: &Value) -> String {
    extra
        .get("title")
        .or_else(|| extra.get("id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "extra".to_string())
}

fn rate_window_label(window: &Value, slot: &str) -> String {
    if let Some(minutes) = window.get("windowMinutes").and_then(Value::as_i64) {
        return label_from_window_minutes(minutes);
    }
    slot.to_string()
}

fn label_from_window_minutes(minutes: i64) -> String {
    match minutes {
        m if m >= 1440 && m % 1440 == 0 => {
            let days = m / 1440;
            if days == 7 {
                "week".to_string()
            } else if days == 1 {
                "day".to_string()
            } else {
                format!("{days}d")
            }
        }
        m if m >= 60 && m % 60 == 0 => format!("{}h", m / 60),
        _ => format!("{minutes}m"),
    }
}

fn format_used_percent_rate_window(window: &Value) -> String {
    let used = window.get("usedPercent").and_then(Value::as_f64);
    match used {
        Some(value) => {
            let rounded = (value * 10.0).round() / 10.0;
            if (rounded - rounded.round()).abs() < f64::EPSILON {
                format!("{:.0}", rounded)
            } else {
                format!("{rounded:.1}")
            }
        }
        None => "-".to_string(),
    }
}

fn format_resets_at_rate_window(window: &Value) -> String {
    let raw = window
        .get("resetsAt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let Some(raw) = raw else {
        return "-".to_string();
    };
    format_reset_timestamp(&raw).unwrap_or(raw)
}

fn format_reset_timestamp(raw: &str) -> Option<String> {
    let secs = parse_rfc3339_to_utc_secs(raw)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let local = utc_parts_from_epoch_secs(secs);
    let now_local = utc_parts_from_epoch_secs(now);
    if local.year == now_local.year && local.month == now_local.month && local.day == now_local.day
    {
        Some(format!("{:02}:{:02}", local.hour, local.minute))
    } else {
        Some(format!(
            "{} {:02} {:02}:{:02}",
            month_abbr(local.month),
            local.day,
            local.hour,
            local.minute
        ))
    }
}

fn parse_rfc3339_to_utc_secs(raw: &str) -> Option<u64> {
    if raw.len() < 19 {
        return None;
    }
    let bytes = raw.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i32 = raw[0..4].parse().ok()?;
    let month: u32 = raw[5..7].parse().ok()?;
    let day: u32 = raw[8..10].parse().ok()?;
    let hour: u32 = raw[11..13].parse().ok()?;
    let minute: u32 = raw[14..16].parse().ok()?;
    let second: u32 = raw[17..19].parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    // Skip fractional seconds (".466665") — some providers emit them.
    let mut rest = &raw[19..];
    if rest.starts_with('.') {
        let end = rest[1..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    let rest = rest.trim_start();
    let offset_secs = if rest.is_empty() || rest.starts_with('Z') || rest.starts_with('z') {
        0
    } else if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let tail = &rest[1..];
        let (oh, om) = parse_hh_mm_offset(tail)?;
        let mag = (oh as i64) * 3600 + (om as i64) * 60;
        if sign == '+' {
            -mag
        } else {
            mag
        }
    } else {
        return None;
    };

    let days = civil_to_days(year, month, day)?;
    let secs_of_day = (hour as u64) * 3600 + (minute as u64) * 60 + (second as u64);
    let utc = (days as i64) * 86_400 + secs_of_day as i64 + offset_secs;
    if utc < 0 {
        return None;
    }
    Some(utc as u64)
}

fn parse_hh_mm_offset(tail: &str) -> Option<(u32, u32)> {
    let (h, m) = if let Some((h, m)) = tail.split_once(':') {
        (h, m)
    } else if tail.len() >= 4 {
        (&tail[..2], &tail[2..])
    } else {
        return None;
    };
    let oh: u32 = h.parse().ok()?;
    let om: u32 = m.parse().ok()?;
    if oh > 23 || om > 59 {
        return None;
    }
    Some((oh, om))
}

fn civil_to_days(year: i32, month: u32, day: u32) -> Option<i32> {
    let (y, m) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m as i32 - 3) + 2) / 5 + day as i32 - 1 + yoe * 365 + yoe / 4 - yoe / 100;
    Some(era * 146097 + doy - 719468)
}

struct LocalTimeParts {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
}

fn utc_parts_from_epoch_secs(secs: u64) -> LocalTimeParts {
    let days = (secs / 86_400) as i32;
    let rem = (secs % 86_400) as u32;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let (year, month, day) = civil_from_days(days);
    LocalTimeParts {
        year,
        month,
        day,
        hour,
        minute,
    }
}

fn civil_from_days(mut z: i32) -> (i32, u32, u32) {
    z += 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

fn month_abbr(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
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
                display_field(module, "health"),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["id", "state", "enabled", "live", "health"], rows);
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
            health_status,
            failures,
            last_action,
            detail,
            truncate_cell(&metrics),
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
                display_field(module, "detail"),
                module
                    .get("metrics")
                    .map(display_json_value)
                    .map(|metrics| truncate_cell(&metrics))
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
            "detail",
            "metrics",
        ],
        rows,
    );
}

/// Cap a table cell so one module's large opaque metrics blob cannot make the
/// whole table unreadable; `--json` is the full-fidelity view.
fn truncate_cell(cell: &str) -> String {
    const MAX: usize = 120;
    if cell.chars().count() <= MAX {
        return cell.to_string();
    }
    let head: String = cell.chars().take(MAX).collect();
    format!("{head}… (--json for full)")
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
        [domain] if domain == "quota" => Ok(Command::Quota { provider_id: None }),
        [domain, provider_id] if domain == "quota" => Ok(Command::Quota {
            provider_id: Some(provider_id.clone()),
        }),
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
