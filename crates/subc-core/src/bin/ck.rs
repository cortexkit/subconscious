#![forbid(unsafe_code)]

//! `ck` — the CortexKit operator CLI.
//!
//! This binary is the founding piece of the CortexKit umbrella command. The
//! daemon/module control domain ships first, and the argument parser is shaped as
//! a small `<domain> <verb>` dispatcher so future domains such as `ck vault ...`,
//! `ck quota ...`, and `ck account ...` can be added without reshaping the CLI.

use std::{
    collections::{BTreeMap, HashMap},
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, IsTerminal},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use subc_control::{CatalogEntry, ClientControlRequest, ClientControlResponse};
// The connection-file name embeds a per-user token. `ck` must derive it the same
// way the daemon does, so it imports the daemon's function rather than carrying
// a copy -- these two used to be byte-identical duplicates in different files,
// with nothing asserting they agreed.
use subc_core::bootstrap::user_connection_token;
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
const QUOTA_MODULE_ID: &str = "insula";
const CK_HARNESS: &str = "ck";

const TOP_HELP_BASE: &str = "ck — CortexKit operator CLI\n\nusage:\n  ck [--subc <connection-file>] [--json] <domain> [<verb>] [<args>]\n\ndomains:\n  module    supervised modules: list, status, stderr, restart, stop, start, rescan\n  health    one-line health for every supervised module\n  quota     AI-provider quota and usage windows\n  daemon    daemon version, uptime, and connection info";

const TOP_HELP_TAIL: &str = "flags:\n  --subc <file>   use a specific connection file (default: auto-discover)\n  --json          raw JSON output instead of tables\n\nrun 'ck <domain>' with no verb to see that domain's commands";

/// Top-level help with the externally-dispatched domains discovered from PATH
/// (any executable named ck-<domain>), so 'ck' shows the REAL command surface
/// of this machine, not just the built-ins.
fn top_help() -> String {
    let external = discover_external_domains();
    let mut out = String::from(TOP_HELP_BASE);
    if external.is_empty() {
        out.push_str("\n\nany other domain dispatches to a ck-<domain> binary on PATH\n\n");
    } else {
        out.push_str("\n\ninstalled domains (dispatched to ck-<domain>):\n");
        for domain in &external {
            out.push_str(&format!("  {domain}\n"));
        }
        out.push('\n');
    }
    out.push_str(TOP_HELP_TAIL);
    out
}

/// Executables named `ck-<domain>` on PATH, deduped and sorted. The `ck-`
/// prefix is also the fleet's supervised-daemon naming convention, so daemon
/// binaries living in module data dirs are naturally absent (not on PATH).
fn discover_external_domains() -> Vec<String> {
    let Some(path_var) = env::var_os("PATH") else {
        return Vec::new();
    };
    let mut domains = Vec::new();
    for dir in env::split_paths(&path_var) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(domain) = name.strip_prefix("ck-") else {
                continue;
            };
            if domain.is_empty() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // fs::metadata (not DirEntry::metadata) so symlinked tools count:
                // installed ck-* binaries are conventionally symlinks into
                // target/release trees.
                let executable = fs::metadata(entry.path())
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false);
                if !executable {
                    continue;
                }
            }
            let domain = domain.strip_suffix(".exe").unwrap_or(domain);
            domains.push(domain.to_string());
        }
    }
    domains.sort();
    domains.dedup();
    domains
}

const MODULE_HELP: &str = "ck module — inspect and control supervised modules\n\nusage: ck [--json] module <verb> [<args>]\n\nverbs:\n  ck module list            all modules with state and health\n  ck module status <id>     one module in detail
  ck module stderr <id>     retained stderr for a module (-n <count> to limit)\n  ck module restart <id>    drain-restart a module\n  ck module stop <id>       disable and stop a module (persists until start)\n  ck module start <id>      enable and spawn a module\n  ck module rescan          re-read subc.jsonc and reconcile the module set\n  ck module rescan --dry-run  show what a rescan would change, without changing it";

const QUOTA_HELP: &str = "ck quota - AI-provider quota and usage windows\n\nusage: ck [--json] quota [--verbose] [<provider-id>]\n\n  ck quota              connected providers and their usage windows\n  ck quota --verbose    all tracked providers, including unavailable ones\n  ck quota claude       one provider's windows and status in detail";

const HEALTH_HELP: &str = "ck health — module health\n\nusage: ck [--json] health [<module-id>]\n\n  ck health            one-line health for every supervised module (cached)\n  ck health <id>       fresh health.check probe with FULL metrics — bypasses\n                       the supervisor cache and its size truncation";

const DAEMON_HELP: &str =
    "ck daemon — daemon version, uptime, and connection info\n\nusage: ck [--json] daemon";

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

    // Help and external dispatch resolve without a daemon connection: help is
    // static text, and an external ck-<domain> tool discovers its own connection.
    if let Command::Help(text) = args.command {
        println!("{text}");
        return Ok(());
    }
    if let Command::External { domain, tail } = args.command {
        return dispatch_external(&domain, &tail);
    }

    let resolved = discover_connection_file(args.subc.as_deref())?;
    let mut client = CkClient::connect(resolved).await?;

    match args.command {
        Command::Module(ModuleCommand::List) => module_list(&mut client, args.json).await,
        Command::Module(ModuleCommand::Status { module_id }) => {
            module_status(&mut client, &module_id, args.json).await
        }
        Command::Module(ModuleCommand::StderrTail {
            module_id,
            max_lines,
        }) => module_stderr_tail(&mut client, &module_id, max_lines, args.json).await,
        Command::Module(ModuleCommand::Restart { module_id }) => {
            module_restart(&mut client, &module_id, args.json).await
        }
        Command::Module(ModuleCommand::Rescan { preview }) => {
            module_rescan(&mut client, args.json, preview).await
        }
        Command::Module(ModuleCommand::Stop { module_id }) => {
            module_set_enabled(&mut client, &module_id, false, args.json).await
        }
        Command::Module(ModuleCommand::Start { module_id }) => {
            module_set_enabled(&mut client, &module_id, true, args.json).await
        }
        Command::Health => health(&mut client, args.json).await,
        Command::HealthDetail { module_id } => {
            health_detail(&mut client, &module_id, args.json).await
        }
        Command::Daemon => daemon(&mut client, args.json).await,
        Command::Quota {
            provider_id,
            verbose,
        } => quota(&mut client, provider_id.as_deref(), args.json, verbose).await,
        Command::Help(_) | Command::External { .. } => unreachable!("handled before connect"),
    }
}

/// Git-style external dispatch: `ck <domain> …` runs `ck-<domain> …` from PATH,
/// passing the tail through verbatim and propagating the child's exit code.
/// Dispatcher-local flags (`--subc`, `--json`) given BEFORE the domain are not
/// forwarded; an external tool parses its own flags from the tail.
fn dispatch_external(domain: &str, tail: &[OsString]) -> Result<(), CkError> {
    let program = format!("ck-{domain}");
    match process::Command::new(&program).args(tail).status() {
        Ok(status) => process::exit(status.code().unwrap_or(1)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(CkError::Usage(format!(
            "unknown domain '{domain}' (no built-in command and no '{program}' on PATH)\n\n{}",
            top_help()
        ))),
        Err(err) => Err(CkError::Message(format!("failed to run {program}: {err}"))),
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
    HealthDetail {
        module_id: String,
    },
    Daemon,
    Quota {
        provider_id: Option<String>,
        verbose: bool,
    },
    /// Explicit help request (bare `ck`, `ck <domain>` with no verb, `ck help …`,
    /// `-h/--help`): prints to stdout and exits 0 without touching the daemon.
    Help(String),
    /// Unknown domain: git-style external dispatch to a `ck-<domain>` binary on
    /// PATH with the remaining args passed through verbatim.
    External {
        domain: String,
        tail: Vec<OsString>,
    },
}

enum ModuleCommand {
    List,
    Status {
        module_id: String,
    },
    Restart {
        module_id: String,
    },
    Rescan {
        preview: bool,
    },
    Stop {
        module_id: String,
    },
    Start {
        module_id: String,
    },
    StderrTail {
        module_id: String,
        max_lines: Option<u32>,
    },
}

struct ResolvedConnection {
    path: PathBuf,
    info: ConnectionInfo,
}

#[derive(Clone, Copy)]
struct RouteHandle {
    channel: u16,
    epoch: u32,
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
    ) -> Result<RouteHandle, CkError> {
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
            admission_facts: None,
        };
        let value = self.rpc_value(request).await?;
        match serde_json::from_value::<ClientControlResponse>(value)? {
            ClientControlResponse::RouteOpen {
                route_channel,
                route_epoch,
            } => Ok(RouteHandle {
                channel: route_channel,
                epoch: route_epoch,
            }),
            other => Err(CkError::Message(format!(
                "unexpected route.open response: {other:?}"
            ))),
        }
    }

    async fn route_request_value(
        &mut self,
        route: RouteHandle,
        body: Value,
    ) -> Result<Value, CkError> {
        let corr = self.next_corr;
        self.next_corr = self.next_corr.saturating_add(1);
        let body = serde_json::to_vec(&body)?;
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            route.channel,
            route.epoch,
            corr,
            body,
        )
        .map_err(|source| CkError::Message(source.to_string()))?;
        write_frame(&mut self.stream, &frame)
            .await
            .map_err(|source| CkError::Message(source.to_string()))?;

        loop {
            let reply = self.next_frame().await?;
            if reply.header.channel != route.channel
                || reply.header.epoch != route.epoch
                || reply.header.corr != corr
            {
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

    async fn route_goodbye(&mut self, route: RouteHandle) {
        let frame = match Frame::build(
            FrameType::Goodbye,
            Flags::new(false, Priority::Passive, false),
            route.channel,
            route.epoch,
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

/// Read `-n <count>` from a verb's own tail.
///
/// Scoped to the verb rather than the global argument set, like `--dry-run` on
/// rescan, so it cannot silently apply somewhere else.
fn parse_tail_count(tail: &[std::ffi::OsString]) -> Result<Option<u32>, CkError> {
    let Some(position) = tail.iter().position(|arg| arg == "-n") else {
        return Ok(None);
    };
    let raw = tail
        .get(position + 1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .ok_or_else(|| {
            CkError::Usage(format!(
                "ck module stderr -n needs a count\n\n{MODULE_HELP}"
            ))
        })?;
    raw.parse::<u32>()
        .map(Some)
        .map_err(|_| CkError::Usage(format!("ck module stderr -n needs a number, got '{raw}'")))
}

async fn module_stderr_tail(
    client: &mut CkClient,
    module_id: &str,
    max_lines: Option<u32>,
    json_output: bool,
) -> Result<(), CkError> {
    let response = client
        .rpc_value(ClientControlRequest::SupervisorStderrTail {
            module_id: module_id.to_string(),
            max_lines,
            max_bytes: None,
        })
        .await?;

    if json_output {
        print_json(&response)?;
        return Ok(());
    }

    // An uncaptured tail is reported instead of the lines, never alongside them:
    // the entries under that state carry no information about what the module
    // wrote, and printing them under a warning invites reading them as complete.
    let capture = response.get("capture");
    if capture
        .and_then(|capture| capture.get("state"))
        .and_then(Value::as_str)
        .is_some_and(|state| state == "not_captured")
    {
        let reason = capture
            .and_then(|capture| capture.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        println!("stderr not captured for {module_id}: {reason}");
        return Ok(());
    }
    let incomplete_reason = capture
        .filter(|capture| {
            capture
                .get("state")
                .and_then(Value::as_str)
                .is_some_and(|state| state == "incomplete")
        })
        .and_then(|capture| capture.get("reason"))
        .and_then(Value::as_str);

    let dropped = response
        .get("dropped_lines")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if dropped > 0 {
        // Printed BEFORE the lines: a reader scanning for a cause needs to know
        // the first line shown is not the first line written, and a footer after
        // a long tail is read too late to change how the tail is read.
        println!("... {dropped} earlier line(s) dropped");
    }

    let entries = response
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("(no stderr output captured)");
    } else {
        for entry in entries {
            match entry.get("kind").and_then(Value::as_str) {
                Some("process_start") => println!("--- process start ---"),
                _ => {
                    let text = entry
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let truncated = entry
                        .get("truncated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if truncated {
                        println!("{text} [truncated]");
                    } else {
                        println!("{text}");
                    }
                }
            }
        }
    }
    if let Some(reason) = incomplete_reason {
        println!("stderr capture incomplete for {module_id}: {reason}");
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

async fn module_rescan(
    client: &mut CkClient,
    json_output: bool,
    preview: bool,
) -> Result<(), CkError> {
    let result = client
        .rpc_value(ClientControlRequest::SupervisorRescan { preview })
        .await?;

    // A daemon predating the preview field IGNORES it -- serde drops unknown
    // fields -- and runs a REAL rescan, retiring modules the operator was told
    // would only be reported. Measured rather than theorised: the first live
    // --dry-run against the running daemon executed a full reconciliation.
    //
    // So the response must PROVE the daemon honoured the request. It echoes
    // preview:true only from the path that returns before mutating; an older
    // daemon cannot produce that field at all. Absence therefore means the
    // operation may already have applied, and the only honest report is a loud
    // one -- a silent success here is the exact failure the flag exists to
    // prevent.
    let honoured = result
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if preview && !honoured {
        return Err(CkError::Usage(
            "this daemon does not support `rescan --dry-run` and IGNORED the flag: it may \
             have applied a real reconciliation just now. Compare `ck module list` against \
             your config, and upgrade the daemon before relying on --dry-run."
                .to_string(),
        ));
    }

    if json_output {
        print_json(&result)?;
    } else {
        print_rescan_table(&result);
    }
    Ok(())
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
                "daemon".to_string(),
                Value::String(client.path.display().to_string()),
            );
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
        // Name the daemon that actually served this. A mis-targeted mutation
        // otherwise reports success against a different daemon than intended and
        // nothing in the output says so -- the command is loud, correct-looking,
        // and about the wrong subject, which is the hardest kind of mistake to
        // notice because there is no error to see.
        println!("daemon: {}", client.path.display());
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

/// `ck health <id>` — issue a FRESH health.check to the module (via the
/// daemon's supervisor.health_probe one-shot) and render the full report.
/// The probe path carries the module's complete metrics object; nothing
/// passes through the supervisor's cached-status blob or its size cap.
async fn health_detail(
    client: &mut CkClient,
    module_id: &str,
    json_output: bool,
) -> Result<(), CkError> {
    let value = client
        .rpc_value(ClientControlRequest::SupervisorHealthProbe {
            module_id: module_id.to_string(),
        })
        .await?;
    if json_output {
        print_json(&value)?;
        return Ok(());
    }
    let status = value.get("status").and_then(Value::as_str).unwrap_or("?");
    println!("{module_id}: {status}");
    if let Some(detail) = value.get("detail").and_then(Value::as_str) {
        if !detail.is_empty() {
            println!("  {detail}");
        }
    }
    // Say so when a module published nothing, rather than printing a bare
    // status line. The operator ran this verb to see metrics, so silence is
    // read as "nothing to report" when it is equally the shape of a module
    // that publishes no metrics at all and of a reporting path that regressed.
    // Naming the absence does not distinguish those two, but it stops the
    // third reading -- that metrics were seen and were unremarkable -- which
    // is the one a bare `module: ok` invites.
    match value.get("metrics") {
        Some(metrics) if !metrics.is_null() => print_metrics_tree(metrics, 1),
        _ => println!("  (module published no metrics on this probe)"),
    }
    Ok(())
}

/// Render a metrics JSON object as an indented tree. Health metrics are
/// module-defined free-form JSON; a tree keeps nested sections (memory
/// roots, dispatch lanes) readable without knowing their schema. Three
/// readability rules on top of the raw structure: strings print unquoted,
/// small all-scalar objects collapse onto one line, and array items that
/// carry an identity-ish field (project_root, id, name, …) print that
/// identity as the item header instead of an anonymous dash.
fn print_metrics_tree(value: &Value, depth: usize) {
    let indent = "  ".repeat(depth);
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                print_metrics_entry(key, child, depth);
            }
        }
        Value::Array(items) => {
            for item in items {
                print_metrics_array_item(item, depth);
            }
        }
        other => println!("{indent}{}", scalar_text(other)),
    }
}

fn print_metrics_entry(key: &str, child: &Value, depth: usize) {
    let indent = "  ".repeat(depth);
    match child {
        Value::Object(map) => {
            if let Some(inline) = inline_scalar_object(map) {
                println!("{indent}{key}: {inline}");
            } else {
                println!("{indent}{key}:");
                print_metrics_tree(child, depth + 1);
            }
        }
        Value::Array(items) if items.is_empty() => println!("{indent}{key}: []"),
        Value::Array(_) => {
            println!("{indent}{key}:");
            print_metrics_tree(child, depth + 1);
        }
        other => println!("{indent}{key}: {}", scalar_text(other)),
    }
}

fn print_metrics_array_item(item: &Value, depth: usize) {
    let indent = "  ".repeat(depth);
    match item {
        Value::Object(map) => {
            // Lead with the item's identity so a list of roots reads as a
            // list of roots, not a list of anonymous dashes.
            const IDENTITY_KEYS: [&str; 6] =
                ["project_root", "id", "name", "module_id", "path", "root"];
            let identity = IDENTITY_KEYS
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_str).map(|v| (*k, v)));
            if let Some((id_key, id_value)) = identity {
                println!("{indent}- {id_value}");
                for (key, child) in map {
                    if key != id_key {
                        print_metrics_entry(key, child, depth + 1);
                    }
                }
            } else if let Some(inline) = inline_scalar_object(map) {
                println!("{indent}- {inline}");
            } else {
                println!("{indent}-");
                print_metrics_tree(item, depth + 1);
            }
        }
        other => println!("{indent}- {}", scalar_text(other)),
    }
}

/// Collapse an all-scalar object onto one line when it stays short:
/// `bash: pending_completions=0 · running=0`. Anything nested or long
/// keeps the tree form.
fn inline_scalar_object(map: &serde_json::Map<String, Value>) -> Option<String> {
    if map.is_empty() {
        return Some("{}".to_string());
    }
    let mut parts = Vec::with_capacity(map.len());
    for (key, value) in map {
        match value {
            Value::Object(_) | Value::Array(_) => return None,
            other => parts.push(format!("{key}={}", scalar_text(other))),
        }
    }
    let line = parts.join(" · ");
    (line.chars().count() <= 88).then_some(line)
}

/// Scalar leaf rendering: strings unquoted (these are human-facing labels
/// and paths, not re-parseable JSON — `--json` serves that need).
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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
        if let Some(counters) = connected_clients.get("counters").and_then(Value::as_object) {
            let mut rows = counters
                .iter()
                .map(|(name, value)| vec![name.clone(), display_json_value(value)])
                .collect::<Vec<_>>();
            rows.sort_by(|left, right| left[0].cmp(&right[0]));
            print_table(&["counter", "value"], rows);
        }
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
    verbose: bool,
) -> Result<(), CkError> {
    ensure_quota_module_registered(client).await?;
    let project_root = env::current_dir()
        .map_err(|source| CkError::Message(format!("current directory: {source}")))?;
    let route = client
        .route_open_management(QUOTA_MODULE_ID, project_root)
        .await?;
    let body = client
        .route_request_value(route, json!({ "method": "usage.get", "params": {} }))
        .await?;
    client.route_goodbye(route).await;

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
        print_quota_table(&providers, provider_filter, verbose);
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

const QUOTA_PROGRESS_BAR_WIDTH: usize = 16;

fn account_label(entry: &Value) -> String {
    entry
        .get("account")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

fn table_account_label(entry: &Value) -> String {
    shorten_uuid_label(&account_label(entry))
}

fn shorten_uuid_label(label: &str) -> String {
    if is_uuid_shaped(label) {
        label[..8].to_string()
    } else {
        label.to_string()
    }
}

fn is_uuid_shaped(label: &str) -> bool {
    let bytes = label.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn entry_error_detail(entry: &Value) -> Option<String> {
    entry
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn quota_entry_is_connected(entry: &Value) -> bool {
    // Connected is signalled by the presence of a usage object on the wire; a
    // disconnected provider carries an error string and no usage object. The
    // wire carries no explicit "ok" flag, so usage presence is the signal.
    entry.get("usage").is_some_and(Value::is_object)
}

/// What, if anything, a reader should do about a disconnected provider.
///
/// Not-connected used to be one number covering unrelated situations. A provider
/// nobody ever configured is a permanent, correct state; a provider whose
/// credential broke this morning is a login away from working. Counted together,
/// a provider that STOPPED working moves the total by one and produces no other
/// signal — which is how a quota exhaustion went unnoticed here before.
///
/// The split is three ways rather than two because the middle bucket carries an
/// implied instruction. "Configured but failing" means go fix your credential,
/// and that is the wrong thing to tell someone when the fault is in the quota
/// module itself: nothing they can log into or reconfigure changes the outcome.
/// A bucket whose implied action cannot work is a worse place to be than an
/// unlabelled one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaDisconnectKind {
    /// Permanent and correct. Never configured, or the account genuinely has no
    /// quota to report. Nothing to do, and it must never inflate a count that is
    /// supposed to mean "something needs attention".
    Inert,
    /// A person can fix this — usually by logging in again.
    UserFixable,
    /// The quota module itself failed. Real, worth surfacing, and NOT the
    /// reader's to fix.
    ModuleDefect,
}

/// Classify a disconnected entry from the producer's `errorClass` (see the
/// field's docs in `cortexkit-provider-usage`).
///
/// An UNRECOGNISED class is [`UserFixable`](QuotaDisconnectKind::UserFixable) on
/// purpose. The class list is open and grows on the producer's side, so the
/// choice is between surfacing something we have not heard of and silently
/// filing it under "nothing to do". On an observability surface the first is a
/// line someone reads once; the second is the exact blindness this split exists
/// to remove.
///
/// An entry with NO class — any producer predating the field — is `Inert`, so an
/// older producer renders exactly as it did before rather than turning every
/// disconnected provider into an alarm.
fn quota_disconnect_kind(entry: &Value) -> QuotaDisconnectKind {
    match entry.get("errorClass").and_then(Value::as_str) {
        None => QuotaDisconnectKind::Inert,
        Some("credential_absent" | "no_quota_reported") => QuotaDisconnectKind::Inert,
        Some("internal_error") => QuotaDisconnectKind::ModuleDefect,
        Some(_) => QuotaDisconnectKind::UserFixable,
    }
}

fn quota_entries_for_table<'a>(
    providers: &'a [Value],
    filter: Option<&str>,
    verbose: bool,
) -> Vec<&'a Value> {
    let mut entries = providers.iter().collect::<Vec<_>>();
    entries.sort_by_key(|entry| provider_id(entry));
    entries
        .into_iter()
        .filter(|entry| {
            let matches_filter =
                filter.is_none() || filter.is_some_and(|wanted| provider_id(entry) == wanted);
            matches_filter && (filter.is_some() || verbose || quota_entry_is_connected(entry))
        })
        .collect()
}

fn print_quota_table(providers: &[Value], filter: Option<&str>, verbose: bool) {
    let color_enabled = ansi_color_enabled();
    let entries = quota_entries_for_table(providers, filter, verbose);

    // Group by provider so each provider renders as one section with its
    // accounts beneath it, mirroring the breakdown layout users know from
    // oh-my-pi's usage CLI.
    let mut order: Vec<String> = Vec::new();
    let mut grouped: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for entry in entries {
        let id = provider_id(entry);
        if !grouped.contains_key(&id) {
            order.push(id.clone());
        }
        grouped.entry(id).or_default().push(entry);
    }

    println!("{}", bold_text("Usage", color_enabled));

    // An empty provider array is never "nothing configured": a host with no
    // usable credentials still returns a full array of unavailable entries, so
    // the only way to reach zero is a cold module or a structural failure
    // upstream. Saying so beats printing a bare header that reads as "all
    // quiet".
    if order.is_empty() {
        println!();
        let reason = quota_empty_reason(providers.is_empty(), filter.is_some());
        println!("{}", dim_text(reason, color_enabled));
        return;
    }

    for id in order {
        let group = &grouped[&id];
        let connected: Vec<&&Value> = group
            .iter()
            .filter(|entry| quota_entry_is_connected(entry))
            .collect();
        let account_word = if group.len() == 1 {
            "account"
        } else {
            "accounts"
        };
        println!();
        println!(
            "{} {}",
            color_text(&format_provider_display_name(&id), "1;36", color_enabled),
            dim_text(&format!("— {} {account_word}", group.len()), color_enabled)
        );

        // A shared label template across the provider's accounts keeps window
        // rows aligned and makes a window one account reports and another
        // doesn't visible as an explicit "not reported" row.
        let templates = quota_window_templates(group);
        let label_width = templates
            .iter()
            .map(|label| label.chars().count())
            .max()
            .unwrap_or(0);

        for entry in group {
            print_quota_account(entry, &templates, label_width, color_enabled, verbose);
        }

        if connected.len() > 1 {
            let stats = quota_provider_window_stats(group);
            if !stats.is_empty() {
                let parts: Vec<String> = stats
                    .iter()
                    .map(|stat| {
                        let noun = if stat.accounts == 1 {
                            "account"
                        } else {
                            "accounts"
                        };
                        format!(
                            "{} → {:.2}/{} {noun} used ({:.2}× quota left)",
                            stat.window, stat.used_accounts, stat.accounts, stat.remaining_accounts
                        )
                    })
                    .collect();
                println!(
                    "  {}",
                    dim_text(&format!("capacity: {}", parts.join(" · ")), color_enabled)
                );
            }
        }
    }

    if filter.is_none() && !verbose {
        let disconnected = providers
            .iter()
            .filter(|entry| !quota_entry_is_connected(entry))
            .count();
        if disconnected > 0 {
            // Split the count only where the producer gives us the reason. A
            // producer predating `errorClass` classifies everything as inert and
            // renders the single line it always did.
            let kinds: Vec<_> = providers
                .iter()
                .filter(|entry| !quota_entry_is_connected(entry))
                .map(quota_disconnect_kind)
                .collect();
            let count = |kind| kinds.iter().filter(|k| **k == kind).count();
            let failing = count(QuotaDisconnectKind::UserFixable);
            let broken = count(QuotaDisconnectKind::ModuleDefect);

            println!();
            let mut parts = vec![format!(
                "{} not connected",
                count(QuotaDisconnectKind::Inert)
            )];
            if failing > 0 {
                parts.push(format!("{failing} configured but failing"));
            }
            // Named for the culprit rather than the symptom: a reader who tries
            // to fix their own credential here is being sent to the wrong place.
            if broken > 0 {
                parts.push(format!("{broken} quota-module defect"));
            }
            let summary = if failing == 0 && broken == 0 {
                format!("{disconnected} providers not connected (--verbose to list)")
            } else {
                format!("{} (--verbose to list)", parts.join(" · "))
            };
            println!("{}", dim_text(&summary, color_enabled));
        }
    }
}

/// Status classification for the colored dots, matching the progress-bar
/// color thresholds so the dot and the bar never disagree.
fn quota_status_color(used_percent: f64) -> &'static str {
    if used_percent >= 100.0 {
        "31"
    } else if used_percent >= 80.0 {
        "33"
    } else {
        "32"
    }
}

fn quota_entry_worst_used(entry: &Value) -> Option<f64> {
    quota_window_rows_for_entry(entry)
        .iter()
        .filter_map(|(_, window)| quota_window_used_percent(window))
        .fold(None, |acc, used| {
            Some(acc.map_or(used, |max: f64| max.max(used)))
        })
}

/// Why the table came out empty. Separated from rendering so the three cases
/// stay distinguishable: an empty wire array means the module answered with
/// nothing at all, which is cold-or-structural rather than a quiet host.
fn quota_empty_reason(wire_array_empty: bool, filtered: bool) -> &'static str {
    match (wire_array_empty, filtered) {
        (true, _) => "no providers reported - the quota module may still be starting",
        (false, true) => "no accounts matched that provider",
        (false, false) => "no connected accounts (--verbose to list unavailable providers)",
    }
}

fn quota_window_used_percent(window: &Value) -> Option<f64> {
    // rawUsedPercent is the provider's real utilization when a banked-reset
    // relaxed window is in effect; prefer it so 0% effective pacing never
    // reads as an idle account.
    window
        .get("rawUsedPercent")
        .and_then(Value::as_f64)
        .or_else(|| window.get("usedPercent").and_then(Value::as_f64))
}

fn print_quota_account(
    entry: &Value,
    templates: &[String],
    label_width: usize,
    color_enabled: bool,
    verbose: bool,
) {
    // The email is the human identity when the wire carries it; the vault
    // account id (shortened) is the fallback, and the credential source is
    // the last resort so a row is never label-less.
    let mut label = entry
        .get("accountInfo")
        .and_then(|i| i.get("email"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| table_account_label(entry));
    if label.is_empty() {
        label = entry
            .get("source")
            .and_then(Value::as_str)
            .map(|source| format!("{source} account"))
            .unwrap_or_else(|| "account".to_string());
    }

    if !quota_entry_is_connected(entry) {
        let reason = entry_error_detail(entry).unwrap_or_else(|| "no usage data".to_string());
        // "Which ones" is the first question after seeing the failing count, so
        // the class is named here rather than left to the prose. The prose is the
        // producer's human message and carries no stability promise; the class is
        // the stable name, and printing both means an unrecognised class still
        // arrives with a readable explanation beside it.
        let detail = match (
            entry.get("errorClass").and_then(Value::as_str),
            quota_disconnect_kind(entry),
        ) {
            (_, QuotaDisconnectKind::ModuleDefect) => {
                format!("{label} [quota-module defect] — {}", truncate_cell(&reason))
            }
            (Some(class), QuotaDisconnectKind::UserFixable) => {
                format!("{label} [{class}] — {}", truncate_cell(&reason))
            }
            _ => format!("{label} — {}", truncate_cell(&reason)),
        };
        println!(
            "  {} {}",
            dim_text("○", color_enabled),
            dim_text(&detail, color_enabled)
        );
        return;
    }

    let dot_color = quota_entry_worst_used(entry).map(quota_status_color);
    let dot = match dot_color {
        Some(color) => color_text("●", color, color_enabled),
        None => dim_text("●", color_enabled),
    };
    let mut header = format!("  {dot} {}", bold_text(&label, color_enabled));
    for extra in quota_account_header_extras(entry) {
        header.push_str(&dim_text(&format!(" · {extra}"), color_enabled));
    }
    println!("{header}");

    // A connected account can still carry a degraded-path error (one probe
    // arm failing while others serve). The default view keeps it quiet;
    // --verbose surfaces it under the account header.
    if verbose {
        if let Some(detail) = entry_error_detail(entry) {
            println!(
                "      {}",
                dim_text(&format!("⚠ {}", truncate_cell(&detail)), color_enabled)
            );
        }
    }

    let rows = quota_window_rows_for_entry(entry);
    if rows.is_empty() {
        println!("      {}", dim_text("no limits reported", color_enabled));
        return;
    }
    let by_label: HashMap<&str, &Value> = rows
        .iter()
        .map(|(label, window)| (label.as_str(), window))
        .collect();
    for template in templates {
        match by_label.get(template.as_str()) {
            Some(window) => {
                println!(
                    "{}",
                    format_quota_window_line(template, window, label_width, color_enabled)
                );
            }
            None => {
                println!(
                    "      {} {:<label_width$}  {}  {}",
                    dim_text("○", color_enabled),
                    template,
                    dim_text(&"·".repeat(QUOTA_PROGRESS_BAR_WIDTH), color_enabled),
                    dim_text("not reported", color_enabled)
                );
            }
        }
    }
}

fn format_quota_window_line(
    label: &str,
    window: &Value,
    label_width: usize,
    color_enabled: bool,
) -> String {
    let Some(used) = quota_window_used_percent(window) else {
        return format!(
            "      {} {:<label_width$}  {}  {}",
            dim_text("○", color_enabled),
            label,
            dim_text(&"·".repeat(QUOTA_PROGRESS_BAR_WIDTH), color_enabled),
            dim_text("no data", color_enabled)
        );
    };
    let dot = color_text("●", quota_status_color(used), color_enabled);
    let bar = format_quota_progress_bar(used, color_enabled);
    let details = quota_window_details(window);
    format!(
        "      {dot} {label:<label_width$}  {bar}  {}",
        dim_text(&details, color_enabled)
    )
}

/// The human detail string after the bar: real utilization, the effective
/// pacing note for relaxed windows, and a relative reset time.
fn quota_window_details(window: &Value) -> String {
    let mut parts = Vec::new();
    let used = window.get("usedPercent").and_then(Value::as_f64);
    let raw = window.get("rawUsedPercent").and_then(Value::as_f64);
    match (used, raw) {
        (Some(effective), Some(raw)) => {
            parts.push(format!(
                "{}% used ({}% eff · resets banked)",
                format_used_percent(raw),
                format_used_percent(effective)
            ));
        }
        (Some(value), None) => parts.push(format!("{}% used", format_used_percent(value))),
        (None, Some(raw)) => parts.push(format!("{}% used", format_used_percent(raw))),
        (None, None) => parts.push("no data".to_string()),
    }
    if let Some(counts) = quota_window_counts(window) {
        parts.push(counts);
    }
    if let Some(relative) = quota_resets_relative(window) {
        parts.push(format!("resets in {relative}"));
    } else {
        let absolute = format_resets_at_rate_window(window);
        if absolute != "-" {
            parts.push(format!("resets {absolute}"));
        }
    }
    parts.join(" · ")
}

/// Absolute consumed/total ("10,336 / 40,000") when the provider reports
/// counts (cortexkit-provider-usage 0.3.0 usedCount/totalCount).
fn quota_window_counts(window: &Value) -> Option<String> {
    let used = window.get("usedCount").and_then(Value::as_f64)?;
    let total = window.get("totalCount").and_then(Value::as_f64);
    let fmt = |v: f64| -> String {
        let rounded = v.round() as i64;
        // Thousands separators for readability at token scale.
        let raw = rounded.abs().to_string();
        let sep: String = raw
            .as_bytes()
            .rchunks(3)
            .rev()
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(",");
        if rounded < 0 {
            format!("-{sep}")
        } else {
            sep
        }
    };
    match total {
        Some(total) => Some(format!("{} / {}", fmt(used), fmt(total))),
        None => Some(fmt(used)),
    }
}

/// Relative reset countdown ("4h32m", "5d9h") from the window's resetsAt.
fn quota_resets_relative(window: &Value) -> Option<String> {
    let raw = window.get("resetsAt").and_then(Value::as_str)?;
    let reset_secs = parse_rfc3339_to_utc_secs(raw)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if reset_secs <= now {
        return None;
    }
    Some(format_duration_two_units(reset_secs - now))
}

/// Two-unit duration for countdowns: 5d9h, 4h32m, 32m, 45s.
fn format_duration_two_units(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d{hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h{minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

/// Optional account metadata after the label: org, plan, saved resets, and
/// staleness. Every field is additive on the wire (QTA ships them
/// incrementally), so absence simply omits the segment.
fn quota_account_header_extras(entry: &Value) -> Vec<String> {
    let mut extras = Vec::new();
    let info = entry.get("accountInfo");
    // The email is consumed as the primary label upstream; extras start at
    // the org.
    if let Some(org) = info
        .and_then(|i| i.get("orgName"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        extras.push(org.to_string());
    }
    if let Some(plan) = info
        .and_then(|i| i.get("planType"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        extras.push(format!("plan: {plan}"));
    }
    if let Some(resets) = entry.get("savedResets") {
        let count = resets
            .get("availableCount")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if count > 0 {
            let noun = if count == 1 {
                "saved reset"
            } else {
                "saved resets"
            };
            let mut segment = format!("✦ {count} {noun}");
            if let Some(expires) = resets
                .get("soonestExpiresAt")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_to_utc_secs)
            {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if expires > now {
                    segment.push_str(&format!(
                        " · soonest expires in {}",
                        format_duration_two_units(expires - now)
                    ));
                }
            }
            extras.push(segment);
        }
    }
    if let Some(fetched) = entry
        .get("fetchedAt")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_to_utc_secs)
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Only worth a line when meaningfully stale; a fresh sweep is the
        // normal case and would just be noise on every row.
        if now > fetched + 90 {
            extras.push(format!(
                "fetched {} ago",
                format_duration_two_units(now - fetched)
            ));
        }
    }
    extras
}

/// Distinct window labels across a provider's accounts, in first-seen order,
/// so every account renders the same row set (absent ones as "not reported").
fn quota_window_templates(group: &[&Value]) -> Vec<String> {
    let mut seen = Vec::new();
    for entry in group {
        for (label, _) in quota_window_rows_for_entry(entry) {
            if !seen.contains(&label) {
                seen.push(label);
            }
        }
    }
    seen
}

struct QuotaWindowStat {
    window: String,
    accounts: usize,
    used_accounts: f64,
    remaining_accounts: f64,
}

/// Per-window account-capacity aggregation for multi-account providers: each
/// account contributes its most-burned fraction per window label, so the
/// summary reads as "accounts' worth of quota" burned and left.
fn quota_provider_window_stats(group: &[&Value]) -> Vec<QuotaWindowStat> {
    let mut buckets: Vec<(String, Vec<f64>)> = Vec::new();
    for entry in group {
        let mut account_max: HashMap<String, f64> = HashMap::new();
        for (label, window) in quota_window_rows_for_entry(entry) {
            let Some(used) = quota_window_used_percent(&window) else {
                continue;
            };
            let fraction = (used / 100.0).clamp(0.0, 1.0);
            let current = account_max.entry(label).or_insert(0.0);
            if fraction > *current {
                *current = fraction;
            }
        }
        for (label, fraction) in account_max {
            match buckets.iter_mut().find(|(name, _)| *name == label) {
                Some((_, fractions)) => fractions.push(fraction),
                None => buckets.push((label, vec![fraction])),
            }
        }
    }
    buckets
        .into_iter()
        .filter(|(_, fractions)| fractions.len() > 1)
        .map(|(window, fractions)| {
            let accounts = fractions.len();
            let used_accounts: f64 = fractions.iter().sum();
            QuotaWindowStat {
                window,
                accounts,
                used_accounts,
                remaining_accounts: (accounts as f64 - used_accounts).max(0.0),
            }
        })
        .collect()
}

fn format_provider_display_name(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn bold_text(text: &str, color_enabled: bool) -> String {
    color_text(text, "1", color_enabled)
}

fn color_text(text: &str, code: &str, color_enabled: bool) -> String {
    if color_enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn quota_window_rows_for_entry(entry: &Value) -> Vec<(String, Value)> {
    let mut rows = Vec::new();
    let usage = entry.get("usage").and_then(Value::as_object);
    let Some(usage) = usage else {
        return rows;
    };

    // THE THREE SLOTS ARE POSITIONS, NOT A RANKING, AND THEY CAN HAVE HOLES: each
    // is filled from its own optional upstream field, so `secondary` may be absent
    // while `tertiary` is present. Walk all three unconditionally and never stop at
    // the first gap -- another consumer of this wire shipped a status bar reading
    // 25% for an account whose binding constraint was a weekly at 36%, by treating
    // the first slot as the answer.
    //
    // This loop tolerates holes because it is a filter rather than a search, which
    // was luck rather than intent when it was written. The note exists so that
    // stays a decision: an "optimisation" that breaks on the first absent slot
    // compiles, passes these tests (the fixtures are dense), and reproduces that
    // bug silently.
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

fn format_used_percent(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if (rounded - rounded.round()).abs() < f64::EPSILON {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.1}")
    }
}

fn format_quota_progress_bar(used_percent: f64, color_enabled: bool) -> String {
    let percent = if used_percent.is_finite() {
        used_percent.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let filled = ((percent / 100.0) * QUOTA_PROGRESS_BAR_WIDTH as f64).round() as usize;
    let bar = format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(QUOTA_PROGRESS_BAR_WIDTH - filled)
    );
    if !color_enabled {
        return bar;
    }

    let color = if percent < 60.0 {
        32
    } else if percent <= 85.0 {
        33
    } else {
        31
    };
    format!("\x1b[{color}m{bar}\x1b[0m")
}

fn ansi_color_enabled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn dim_text(text: &str, color_enabled: bool) -> String {
    if color_enabled {
        format!("\x1b[2m{text}\x1b[0m")
    } else {
        text.to_string()
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
    } else {
        let sign = rest.chars().next().filter(|c| *c == '+' || *c == '-')?;
        let tail = &rest[1..];
        let (oh, om) = parse_hh_mm_offset(tail)?;
        let mag = (oh as i64) * 3600 + (om as i64) * 60;
        if sign == '+' {
            -mag
        } else {
            mag
        }
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

fn print_rescan_table(result: &Value) {
    let module_ids = |field: &str| {
        result
            .get(field)
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|ids| !ids.is_empty())
            .unwrap_or_else(|| "-".to_string())
    };
    let rows = vec![
        vec!["added".to_string(), module_ids("added")],
        vec!["removed".to_string(), module_ids("removed")],
        vec![
            "changed-pending-reload".to_string(),
            module_ids("changed_pending_reload"),
        ],
        vec!["enabled-changed".to_string(), module_ids("enabled_changes")],
        vec![
            "unchanged".to_string(),
            result
                .get("unchanged")
                .and_then(Value::as_u64)
                .map(|count| count.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ],
    ];
    print_table(&["change", "modules / count"], rows);

    // Say which operation this was. Without it the CLI reproduces the defect the
    // preview exists to fix: a table of changes that cannot tell the reader
    // whether they HAPPENED. The line goes after the table so it is the last thing
    // read, and it names the applying command so the next step is not a guess.
    if result
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        println!("\npreview only — nothing was changed. Run `ck module rescan` to apply.");
    }

    // Sections rescan cannot apply. Printed AFTER the change table and the
    // preview line, so it is the last thing on screen: it is the only part of
    // this output that requires a further action, and a module whose config did
    // not take crash-loops rather than failing visibly.
    let restart_required = module_ids("restart_required");
    if restart_required != "-" {
        println!(
            "\nRESTART REQUIRED — these config sections changed and rescan cannot apply them: {restart_required}\n\
             Modules depending on them keep running their old config until the daemon restarts."
        );
    }
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
    let last_exit = format_last_exit(module);

    print_table(
        &[
            "id",
            "state",
            "enabled",
            "live",
            "health",
            "failures",
            "last_action",
            "last_exit",
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
            last_exit,
            detail,
            truncate_cell(&metrics),
        ]],
    );
}

/// Render the module's most recent process exit as a compact cell, e.g.
/// `sig9` (SIGKILL), `code101` (panic-abort exit), or `-` when the module has
/// never exited. Survives respawn, so a running module still shows what killed
/// its previous incarnation — the signal that tells a crash-loop apart from a
/// clean restart.
fn format_last_exit(module: &Value) -> String {
    let signal = module.get("last_exit_signal").and_then(Value::as_i64);
    let code = module.get("last_exit_code").and_then(Value::as_i64);
    match (signal, code) {
        (Some(sig), _) => format!("sig{sig}"),
        (None, Some(c)) => format!("code{c}"),
        (None, None) => "-".to_string(),
    }
}

fn print_health_table(modules: &[Value]) {
    let color = ansi_color_enabled();
    let width = terminal_width();

    let id_width = modules
        .iter()
        .map(|module| display_field(module, "module_id").chars().count())
        .max()
        .unwrap_or(0)
        .max("module".len());
    // id + gap + dot + status word + gap; detail wraps in the remainder.
    let status_width = "unresponsive".len();
    let detail_col = id_width + 2 + 2 + status_width + 2;
    let detail_width = width.saturating_sub(detail_col).max(20);

    for module in modules {
        let id = display_field(module, "module_id");
        let status = display_field(module, "status");
        let failures = module
            .get("consecutive_failures")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let last_action = display_field(module, "last_action");

        let (dot_code, status_code) = match status.as_str() {
            "ok" => ("32", "32"),
            "degraded" => ("33", "33"),
            "unresponsive" | "failed" => ("31", "1;31"),
            _ => ("2", "2"),
        };
        let dot = color_text("●", dot_code, color);
        let status_cell = color_text(&format!("{status:<status_width$}"), status_code, color);

        // First line: id, status, and the start of the detail text.
        let mut annotations = Vec::new();
        if failures > 0 {
            annotations.push(format!("{failures} missed probe(s)"));
        }
        if last_action != "-" {
            annotations.push(format!("last action: {last_action}"));
        }
        // This whole table is the supervisor's STORED record, not a probe issued
        // for the question -- so every status here describes some moment in the
        // past. Age is what tells a reader whether that moment was before or
        // after the restart they just performed: a pre-restart record reports the
        // old process, reads as a failed deploy, and invites redeploying
        // something already correct. Shown only past a minute, since a fresh
        // record is the ordinary case and annotating it would train the reader to
        // skip the line. `ck health <id>` needs none of this -- it probes.
        if let Some(age_s) = health_record_age_secs(module) {
            if age_s >= 60 {
                annotations.push(format!(
                    "record {} old",
                    format_duration(Duration::from_secs(age_s))
                ));
            }
        }
        let detail = display_field(module, "detail");
        let mut detail_text = if detail == "-" { String::new() } else { detail };
        if !annotations.is_empty() {
            let joined = annotations.join(" · ");
            if detail_text.is_empty() {
                detail_text = joined;
            } else {
                detail_text = format!("{detail_text} · {joined}");
            }
        }

        let lines = wrap_text(&detail_text, detail_width);
        let first = lines.first().map(String::as_str).unwrap_or("");
        println!("{id:<id_width$}  {dot} {status_cell}  {first}");
        for line in lines.iter().skip(1) {
            println!("{:detail_col$}{line}", "");
        }
    }

    println!(
        "{}",
        dim_text(
            "ck health <id> — full metrics for one module · --json — raw",
            color
        )
    );
}

/// Best-effort terminal width: $COLUMNS, then the tty query, then 100.
fn terminal_width() -> usize {
    if let Some(cols) = env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        if cols >= 40 {
            return cols;
        }
    }
    if let Some((terminal_size::Width(cols), _)) = terminal_size::terminal_size() {
        if cols >= 40 {
            return usize::from(cols);
        }
    }
    100
}

/// Greedy word wrap. Words longer than the width are hard-split so a single
/// unbroken token (a path, a JSON fragment) cannot push past the margin.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if current_len > 0 && current_len + 1 + word_len > width {
            lines.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if word_len > width {
            let mut chars = word.chars().peekable();
            while chars.peek().is_some() {
                let take = if current_len > 0 {
                    if current_len + 1 > width {
                        lines.push(std::mem::take(&mut current));
                        current_len = 0;
                        width
                    } else {
                        current.push(' ');
                        current_len += 1;
                        width - current_len
                    }
                } else {
                    width
                };
                let chunk: String = chars.by_ref().take(take.max(1)).collect();
                current_len += chunk.chars().count();
                current.push_str(&chunk);
                if current_len >= width {
                    lines.push(std::mem::take(&mut current));
                    current_len = 0;
                }
            }
            continue;
        }
        if current_len > 0 {
            current.push(' ');
            current_len += 1;
        }
        current.push_str(word);
        current_len += word_len;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
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
        .map(|header| display_width(header))
        .collect::<Vec<_>>();
    for row in &rows {
        for (idx, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(idx) {
                *width = (*width).max(display_width(cell));
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
        print!(
            "{cell}{}",
            " ".repeat(width.saturating_sub(display_width(cell)))
        );
    }
    println!();
}

fn display_width(text: &str) -> usize {
    let mut chars = text.chars();
    let mut width = 0;
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.next() == Some('[') {
            for sequence_char in chars.by_ref() {
                if sequence_char.is_ascii() && ('@'..='~').contains(&sequence_char) {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}

fn print_json(value: &Value) -> Result<(), CkError> {
    println!("{}", format_json_output(value)?);
    Ok(())
}

fn format_json_output(value: &Value) -> Result<String, CkError> {
    Ok(serde_json::to_string_pretty(value)?)
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

/// How long ago the daemon collected a health entry, in seconds.
///
/// `None` when the module has never been probed or the stamp is unreadable — both
/// mean "cannot say how old this is", which must not render as "fresh". A clock
/// that moved backwards between collection and now also yields `None` rather than
/// a wrapped enormous age.
fn health_record_age_secs(entry: &Value) -> Option<u64> {
    let probed_ms = entry.get("last_probe_ms").and_then(Value::as_u64)?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    now_ms.checked_sub(probed_ms).map(|delta| delta / 1000)
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

    // Dispatcher-local flags are only parsed BEFORE the domain; everything from
    // the first positional on is the command tail (an unknown domain forwards it
    // verbatim to the external ck-<domain> binary, flags and all).
    let domain: String = loop {
        match args.next() {
            None => {
                return Ok(CkArgs {
                    subc,
                    json,
                    command: Command::Help(top_help()),
                })
            }
            Some(arg) if arg == OsStr::new("--subc") => {
                subc = Some(PathBuf::from(take_value(&mut args, "--subc")?));
            }
            Some(arg) if arg == OsStr::new("--json") => json = true,
            Some(arg) if arg == OsStr::new("-h") || arg == OsStr::new("--help") => {
                return Ok(CkArgs {
                    subc,
                    json,
                    command: Command::Help(top_help()),
                })
            }
            Some(arg) if arg == OsStr::new("--version") || arg == OsStr::new("-V") => {
                return Ok(CkArgs {
                    subc,
                    json,
                    command: Command::Help(format!("ck {}", env!("CARGO_PKG_VERSION"))),
                })
            }
            Some(arg) if arg.to_string_lossy().starts_with('-') => {
                return Err(CkError::Usage(format!(
                    "unknown flag '{}'\n\n{}",
                    arg.to_string_lossy(),
                    top_help()
                )))
            }
            Some(arg) => {
                break arg.into_string().map_err(|value| {
                    CkError::Usage(format!(
                        "domain must be UTF-8, got '{}'",
                        value.to_string_lossy()
                    ))
                })?
            }
        }
    };

    let raw_tail: Vec<OsString> = args.collect();

    // Built-in domains accept the dispatcher flags anywhere (`ck module list
    // --subc <file>` is long-standing usage); an external domain's tail is
    // forwarded verbatim so the ck-<domain> tool parses its own flags.
    let tail = if is_builtin_domain(&domain) {
        let mut positionals = Vec::new();
        let mut iter = raw_tail.into_iter();
        while let Some(arg) = iter.next() {
            if arg == OsStr::new("--subc") {
                subc = Some(PathBuf::from(take_value(&mut iter, "--subc")?));
            } else if arg == OsStr::new("--json") {
                json = true;
            } else {
                positionals.push(arg);
            }
        }
        positionals
    } else {
        raw_tail
    };

    let command = parse_command(&domain, &tail)?;
    Ok(CkArgs {
        subc,
        json,
        command,
    })
}

fn is_builtin_domain(domain: &str) -> bool {
    matches!(domain, "module" | "health" | "daemon" | "quota" | "help")
}

fn parse_command(domain: &str, tail: &[OsString]) -> Result<Command, CkError> {
    // Built-in domains parse their verbs strictly and answer verbless/misused
    // invocations with the DOMAIN's help, not the whole command tree.
    match domain {
        "help" => {
            let topic = tail.first().map(|t| t.to_string_lossy());
            Ok(Command::Help(match topic.as_deref() {
                Some("module") => MODULE_HELP.into(),
                Some("quota") => QUOTA_HELP.into(),
                Some("health") => HEALTH_HELP.into(),
                Some("daemon") => DAEMON_HELP.into(),
                _ => top_help(),
            }))
        }
        "module" => {
            let verb = match tail.first() {
                None => return Ok(Command::Help(MODULE_HELP.into())),
                Some(v) => v.to_string_lossy().into_owned(),
            };
            if verb == "-h" || verb == "--help" || verb == "help" {
                return Ok(Command::Help(MODULE_HELP.into()));
            }
            // A HELP REQUEST ANYWHERE IN THE TAIL IS STILL A HELP REQUEST. Checking
            // only the verb position meant `ck module rescan --help` fell through to
            // the verb match and RAN THE RECONCILIATION -- an operator asking a
            // destructive command to explain itself got the command. Placed before
            // the verb match so it cannot be reached by any verb.
            if tail
                .iter()
                .skip(1)
                .any(|t| t == "-h" || t == "--help" || t == "help")
            {
                return Ok(Command::Help(MODULE_HELP.into()));
            }
            let id = |n: usize| -> Result<String, CkError> {
                tail.get(n)
                    .map(|t| t.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        CkError::Usage(format!(
                            "ck module {verb} needs a module id\n\n{MODULE_HELP}"
                        ))
                    })
            };
            let command = match verb.as_str() {
                "list" => ModuleCommand::List,
                // --dry-run computes the reconciliation daemon-side and returns
                // it without applying it. The flag is read from the verb's own
                // tail rather than the global argument set, so it cannot silently
                // apply to a different verb.
                "rescan" => ModuleCommand::Rescan {
                    preview: tail.iter().any(|t| t == "--dry-run"),
                },
                "status" => ModuleCommand::Status { module_id: id(1)? },
                // `-n <count>` narrows the tail daemon-side rather than here, so
                // a caller asking for 20 lines is not shipped the whole ring to
                // discard most of it.
                "stderr" => ModuleCommand::StderrTail {
                    module_id: id(1)?,
                    max_lines: parse_tail_count(tail)?,
                },
                "restart" => ModuleCommand::Restart { module_id: id(1)? },
                "stop" => ModuleCommand::Stop { module_id: id(1)? },
                "start" => ModuleCommand::Start { module_id: id(1)? },
                other => {
                    return Err(CkError::Usage(format!(
                        "unknown verb 'module {other}'\n\n{MODULE_HELP}"
                    )))
                }
            };
            Ok(Command::Module(command))
        }
        "health" => match tail.first() {
            None => Ok(Command::Health),
            Some(argument) => {
                let argument = argument.to_string_lossy();
                if argument == "-h" || argument == "--help" || argument == "help" {
                    Ok(Command::Help(HEALTH_HELP.into()))
                } else {
                    Ok(Command::HealthDetail {
                        module_id: argument.into_owned(),
                    })
                }
            }
        },
        "daemon" => match tail.first() {
            None => Ok(Command::Daemon),
            Some(_) => Ok(Command::Help(DAEMON_HELP.into())),
        },
        "quota" => {
            let mut provider_id = None;
            let mut verbose = false;
            for argument in tail {
                let argument = argument.to_string_lossy();
                if argument == "--verbose" {
                    verbose = true;
                } else if provider_id.is_none() {
                    provider_id = Some(argument.into_owned());
                }
            }
            match provider_id.as_deref() {
                Some("-h") | Some("--help") | Some("help") => Ok(Command::Help(QUOTA_HELP.into())),
                _ => Ok(Command::Quota {
                    provider_id,
                    verbose,
                }),
            }
        }
        _ => Ok(Command::External {
            domain: domain.to_string(),
            tail: tail.to_vec(),
        }),
    }
}

fn take_value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString, CkError> {
    args.next()
        .ok_or_else(|| CkError::Usage(format!("{flag} requires a value; run bare ck for usage")))
}

fn discover_connection_file(override_path: Option<&Path>) -> Result<ResolvedConnection, CkError> {
    let candidates = connection_file_candidates(override_path);
    let mut tried = Vec::new();

    for path in candidates {
        match connection_file::read_for_client(&path) {
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
    connection_file_candidates_with(
        override_path,
        non_empty_os_var("SUBC_CONNECTION_FILE").map(PathBuf::from),
    )
}

/// The candidate list, with the environment-named path passed in rather than read.
///
/// Taking it as a parameter is what makes the exclusivity rule below testable:
/// reading it here would force a test to mutate the process environment, which
/// races under threaded test execution.
fn connection_file_candidates_with(
    override_path: Option<&Path>,
    env_named: Option<PathBuf>,
) -> Vec<PathBuf> {
    if let Some(path) = override_path {
        return vec![path.to_path_buf()];
    }

    // SUBC_CONNECTION_FILE names the daemon the caller means, so it is EXCLUSIVE
    // rather than first-in-a-list. It used to be pushed ahead of the discovery
    // candidates, which reads as honouring it and is not: a path that is set and
    // wrong falls through to discovery and answers from whichever daemon is found
    // -- in practice production. The reply is then true and about the wrong
    // machine, and every later verdict inherits that while the operator believes
    // they are reading a rig.
    //
    // A fallback is only a hazard where the primary is optional, so removing the
    // fallback for a deliberately supplied value removes the class. Returning a
    // single candidate keeps the existing error path: the file is stat-ed, and an
    // unreadable one is reported as a failure naming that path.
    if let Some(only) = env_named {
        return vec![only];
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

fn non_empty_os_var(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_progress_bars_have_fixed_width_at_thresholds() {
        for (percent, filled) in [(0.0, 0), (47.0, 8), (60.0, 10), (85.0, 14), (100.0, 16)] {
            let expected = format!(
                "{}{}",
                "█".repeat(filled),
                "░".repeat(QUOTA_PROGRESS_BAR_WIDTH - filled)
            );
            let actual = format_quota_progress_bar(percent, false);
            assert_eq!(actual, expected, "unexpected bar for {percent}%");
            assert_eq!(display_width(&actual), QUOTA_PROGRESS_BAR_WIDTH);
        }
    }

    #[test]
    fn window_details_include_used_and_total_counts_when_present() {
        let enriched = serde_json::json!({
            "usedPercent": 25.8, "usedCount": 10336.0, "totalCount": 40000.0
        });
        let details = quota_window_details(&enriched);
        assert!(
            details.contains("10,336 / 40,000"),
            "counts must render with separators: {details}"
        );
        // Absent counts leave the line unchanged (no stray separators).
        let plain = serde_json::json!({ "usedPercent": 25.8 });
        let details = quota_window_details(&plain);
        assert!(!details.contains('/'), "no counts, no slash: {details}");
        // used without total renders alone.
        let used_only = serde_json::json!({ "usedPercent": 25.8, "usedCount": 512.0 });
        assert!(quota_window_details(&used_only).contains("512"));
    }

    #[test]
    fn relaxed_window_renders_raw_percent_with_effective_note() {
        // A relaxed (banked-reset) window carries provider truth in
        // rawUsedPercent beside the effective pacing number; the human view
        // must show the raw value, not the effective zero.
        let relaxed = serde_json::json!({ "usedPercent": 0.0, "rawUsedPercent": 70.0 });
        let details = quota_window_details(&relaxed);
        assert!(
            details.contains("70% used"),
            "raw percent missing: {details}"
        );
        assert!(
            details.contains("(0% eff · resets banked)"),
            "effective note missing: {details}"
        );
        // The bar and status dot follow the raw number too.
        assert_eq!(quota_window_used_percent(&relaxed), Some(70.0));

        // Unrelaxed windows omit the field and keep the plain rendering.
        let plain = serde_json::json!({ "usedPercent": 58.0 });
        let details = quota_window_details(&plain);
        assert!(
            details.contains("58% used"),
            "plain percent missing: {details}"
        );
        assert!(!details.contains("eff"), "unexpected note: {details}");
    }

    #[test]
    fn countdown_durations_use_two_units() {
        assert_eq!(format_duration_two_units(16_320), "4h32m");
        assert_eq!(format_duration_two_units(5 * 86_400 + 9 * 3_600), "5d9h");
        assert_eq!(format_duration_two_units(45), "45s");
        assert_eq!(format_duration_two_units(31 * 60), "31m");
        assert_eq!(format_duration_two_units(2 * 3_600), "2h");
    }

    #[test]
    fn window_templates_union_labels_across_accounts_in_first_seen_order() {
        let a = serde_json::json!({
            "provider": "anthropic",
            "usage": {
                "primary": { "usedPercent": 25.0, "windowMinutes": 300 },
                "secondary": { "usedPercent": 54.0, "windowMinutes": 10080 },
                "extraRateWindows": [
                    { "title": "7 Day (Fable)", "window": { "usedPercent": 97.0 } }
                ]
            }
        });
        let b = serde_json::json!({
            "provider": "anthropic",
            "usage": {
                "primary": { "usedPercent": 7.0, "windowMinutes": 300 }
            }
        });
        let group = vec![&a, &b];
        let templates = quota_window_templates(&group);
        assert_eq!(templates, ["5h", "week", "7 Day (Fable)"]);
    }

    #[test]
    fn provider_capacity_stats_sum_binding_fractions_per_window() {
        // Two accounts on the same 5h window at 25% and 7% burn 0.32 accounts'
        // worth of quota, leaving 1.68x; single-account windows are omitted
        // (capacity math is only informative across account multiples).
        let a = serde_json::json!({
            "provider": "anthropic",
            "usage": {
                "primary": { "usedPercent": 25.0, "windowMinutes": 300 },
                "secondary": { "usedPercent": 54.0, "windowMinutes": 10080 }
            }
        });
        let b = serde_json::json!({
            "provider": "anthropic",
            "usage": {
                "primary": { "usedPercent": 7.0, "windowMinutes": 300 }
            }
        });
        let group = vec![&a, &b];
        let stats = quota_provider_window_stats(&group);
        assert_eq!(stats.len(), 1, "only the shared 5h window qualifies");
        let stat = &stats[0];
        assert_eq!(stat.window, "5h");
        assert_eq!(stat.accounts, 2);
        assert!((stat.used_accounts - 0.32).abs() < 1e-9);
        assert!((stat.remaining_accounts - 1.68).abs() < 1e-9);
    }

    #[test]
    fn account_header_extras_are_additive_and_absent_safe() {
        // Bare current-wire entry: no extras at all.
        let bare = serde_json::json!({ "provider": "codex", "account": "291f5165" });
        assert!(quota_account_header_extras(&bare).is_empty());

        // Enriched entry per QTA's committed additive contract.
        let enriched = serde_json::json!({
            "provider": "codex",
            "account": "ufukaltinok@gmail.com",
            "accountInfo": { "email": "ufukaltinok@gmail.com", "planType": "pro" },
            "savedResets": { "availableCount": 4 }
        });
        let extras = quota_account_header_extras(&enriched);
        // email is the primary label upstream, never repeated in extras.
        assert_eq!(extras.len(), 2, "extras: {extras:?}");
        assert_eq!(extras[0], "plan: pro");
        assert!(extras[1].starts_with("✦ 4 saved resets"));
    }

    #[test]
    fn missing_window_renders_as_not_reported_row() {
        let entry = serde_json::json!({
            "provider": "anthropic",
            "account": "wwaxpoetic@yahoo.com",
            "usage": { "primary": { "usedPercent": 7.0, "windowMinutes": 300 } }
        });
        // Render against a template set that includes a window this account
        // does not report; the line must exist and say so rather than vanish.
        let templates = ["5h".to_string(), "7 Day (Fable)".to_string()];
        let rows = quota_window_rows_for_entry(&entry);
        let by_label: Vec<&str> = rows.iter().map(|(label, _)| label.as_str()).collect();
        assert!(by_label.contains(&"5h"));
        assert!(!by_label.contains(&"7 Day (Fable)"));
        // The not-reported arm is exercised through print_quota_account; here
        // we pin the line formatting primitive it uses.
        let line = format_quota_window_line("5h", &rows[0].1, templates[1].len(), false);
        assert!(line.contains("7% used"), "line: {line}");
        assert!(line.contains("●"), "status dot missing: {line}");
    }

    #[test]
    fn window_slots_are_walked_past_a_hole() {
        // The three slots are positions, not a ranking, and each is filled from
        // its own optional upstream field -- so a middle slot can be absent while
        // a later one is present. Every other fixture here is dense, which means
        // a walker that stopped at the first gap would pass all of them.
        let entry = serde_json::json!({
            "provider": "anthropic",
            "usage": {
                "primary": { "usedPercent": 25.0, "windowMinutes": 300 },
                "tertiary": { "usedPercent": 36.0, "windowMinutes": 10080 }
            }
        });
        let rows = quota_window_rows_for_entry(&entry);
        assert_eq!(
            rows.len(),
            2,
            "a hole at `secondary` must not truncate the walk: {rows:?}"
        );
        // Walking past the hole matters because the LATER slot carries the binding
        // constraint: reporting only the first shows 25% for an account limited at
        // 36%, which is the bug another consumer of this wire shipped.
        let worst = rows
            .iter()
            .filter_map(|(_, w)| w.get("usedPercent").and_then(Value::as_f64))
            .fold(f64::MIN, f64::max);
        assert_eq!(worst, 36.0, "binding constraint lost: {rows:?}");
    }

    #[test]
    fn table_account_labels_shorten_only_uuid_shapes() {
        assert_eq!(
            shorten_uuid_label("550e8400-e29b-41d4-a716-446655440000"),
            "550e8400"
        );
        assert_eq!(shorten_uuid_label("work"), "work");
        assert_eq!(
            shorten_uuid_label("not-a-uuid-e29b-41d4-a716-446655440000"),
            "not-a-uuid-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn empty_quota_table_distinguishes_a_silent_module_from_a_quiet_host() {
        // The producer never returns an empty array for "nothing configured": a
        // host with no usable credentials still returns a full array of
        // unavailable entries. So an empty wire array can only be a cold module
        // or a structural failure, and it must not share a message with the
        // case where every provider answered and none were connected.
        let silent_module = quota_empty_reason(true, false);
        let all_unavailable = quota_empty_reason(false, false);
        let filtered_miss = quota_empty_reason(false, true);

        assert_ne!(
            silent_module, all_unavailable,
            "an empty wire array must not read the same as a host whose providers all answered"
        );
        assert_ne!(silent_module, filtered_miss);
        assert_ne!(all_unavailable, filtered_miss);

        // The silent-module case is the only one where something upstream is
        // actually broken, so it has to say so rather than describe the host.
        assert!(
            silent_module.contains("quota module"),
            "empty wire array must name the module, got: {silent_module}"
        );
        assert!(
            !all_unavailable.contains("quota module"),
            "a fully-answered host must not blame the module, got: {all_unavailable}"
        );
    }

    #[test]
    fn quota_default_filters_to_connected_entries_even_without_windows() {
        // The wire signals "connected" by the presence of a usage object, never
        // an explicit ok flag (the real module emits usage OR error, no ok key).
        let providers = vec![
            serde_json::json!({
                "provider": "connected",
                "usage": { "primary": { "usedPercent": 0.0 } }
            }),
            serde_json::json!({
                "provider": "empty-windows",
                "usage": {}
            }),
            serde_json::json!({
                "provider": "unavailable",
                "error": "no session: no API key set"
            }),
            serde_json::json!({
                "provider": "missing-usage"
            }),
        ];

        let default_entries = quota_entries_for_table(&providers, None, false);
        let default_ids = default_entries
            .iter()
            .map(|entry| provider_id(entry))
            .collect::<Vec<_>>();
        assert_eq!(default_ids, ["connected", "empty-windows"]);
        let empty_windows = default_entries
            .iter()
            .find(|entry| provider_id(entry) == "empty-windows")
            .expect("connected empty-window entry");
        assert!(quota_window_rows_for_entry(empty_windows).is_empty());

        assert_eq!(quota_entries_for_table(&providers, None, true).len(), 4);
        assert_eq!(
            quota_entries_for_table(&providers, Some("unavailable"), false).len(),
            1
        );
    }

    #[test]
    fn quota_json_output_preserves_the_raw_reply_format() {
        let reply = serde_json::json!({
            "result": [{
                "provider": "codex",
                "usage": {}
            }]
        });
        let expected =
            "{\n  \"result\": [\n    {\n      \"provider\": \"codex\",\n      \"usage\": {}\n    }\n  ]\n}";
        assert_eq!(format_json_output(&reply).unwrap(), expected);
    }

    /// The whole point of the split is that the second number is trustworthy, so
    /// it must be zero when every degraded provider is degraded for a reason
    /// nobody can act on. A count that is permanently non-zero while nothing is
    /// wrong stops being read within a week.
    #[test]
    fn a_never_configured_provider_is_not_counted_as_failing() {
        for class in ["credential_absent", "no_quota_reported"] {
            let entry = serde_json::json!({
                "provider": "p", "error": "x", "errorClass": class
            });
            assert_eq!(
                quota_disconnect_kind(&entry),
                QuotaDisconnectKind::Inert,
                "{class} must not land in a bucket that implies work"
            );
        }
    }

    /// A credential that broke this morning is the case the split exists to
    /// surface. Exercised across every actionable class the producer ships today
    /// rather than one representative, so a class going quiet is a failure here
    /// rather than a silent drop in the count.
    #[test]
    fn a_broken_credential_is_counted_as_failing() {
        for class in [
            "credential_unusable",
            "credential_rejected",
            "upstream_failed",
            "decode_failed",
        ] {
            let entry = serde_json::json!({
                "provider": "p", "error": "x", "errorClass": class
            });
            assert_eq!(
                quota_disconnect_kind(&entry),
                QuotaDisconnectKind::UserFixable,
                "{class} names something a person can fix"
            );
        }
    }

    /// A connection file named in the environment must be the ONLY candidate.
    ///
    /// It used to be pushed ahead of the discovery paths, which reads as honouring
    /// it and is not: a path that is set and wrong falls through and answers from
    /// whichever daemon discovery finds, in practice production. The reply is then
    /// true and about the wrong machine. This cost a real operation, where a
    /// mistyped rig path reported a production module as healthy one step before a
    /// stop command.
    #[test]
    fn an_environment_named_connection_file_is_the_only_candidate() {
        let named = PathBuf::from("/rig/x.json");
        let candidates = connection_file_candidates_with(None, Some(named.clone()));
        assert_eq!(
            candidates,
            vec![named.clone()],
            "a named connection file must not be followed by discovery paths"
        );

        // Absence must still produce candidates, or discovery could never run and
        // the assertion above would hold for the wrong reason.
        //
        // The property is that discovery RAN and produced something other than the
        // named path -- not how many candidates it found. An earlier version
        // asserted a count above one, which is a Unix-shaped proxy: Windows has no
        // XDG runtime dir and no HOME, so its discovery correctly yields exactly
        // one candidate (the per-user temp path the daemon actually publishes to).
        // The count stood in for the property and disagreed with it on a platform
        // where the code was right.
        let discovered = connection_file_candidates_with(None, None);
        assert!(
            !discovered.is_empty(),
            "without a named file, discovery must offer at least one candidate"
        );
        assert!(
            !discovered.contains(&named),
            "discovery must not reach for the named path it was not given"
        );
    }

    /// A stored health record must disclose its age, because the surface is read
    /// right after a restart to confirm a deploy. A record collected BEFORE the
    /// restart describes the old process; without an age the reader cannot tell
    /// it from a current one, concludes the deploy failed, and redeploys
    /// something that was already correct.
    ///
    /// Never-probed must NOT render as fresh: "no stamp" and "stamped just now"
    /// are opposite facts, and defaulting the absent case to zero would make the
    /// staler of the two look like the newer.
    #[test]
    fn a_health_record_without_a_probe_stamp_cannot_claim_to_be_fresh() {
        let stamped = serde_json::json!({
            "module_id": "m",
            "last_probe_ms": (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
                as u64)
                - 7_200_000
        });
        let age = health_record_age_secs(&stamped).expect("a stamped record has an age");
        assert!(
            (7150..=7250).contains(&age),
            "a two-hour-old record must report about two hours, got {age}s"
        );

        let unstamped = serde_json::json!({ "module_id": "m" });
        assert_eq!(
            health_record_age_secs(&unstamped),
            None,
            "never-probed must be unknown, never zero -- zero renders as fresh"
        );

        // A clock that moved backwards between collection and now yields no age
        // rather than a wrapped enormous one, which would read as a decades-old
        // record and send someone looking for a fault that is not there.
        let future = serde_json::json!({
            "module_id": "m",
            "last_probe_ms": (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
                as u64)
                + 60_000
        });
        assert_eq!(health_record_age_secs(&future), None);
    }

    /// A failure inside the quota module is real and NOT the reader's to fix.
    /// Putting it beside a broken credential would tell them to go re-authorise
    /// something that is working — a bucket whose implied action cannot succeed
    /// is worse than an unlabelled one, because it directs the work confidently.
    #[test]
    fn a_failure_inside_the_quota_module_is_not_blamed_on_the_user() {
        let entry = serde_json::json!({
            "provider": "p", "error": "internal error: provider fetch panicked",
            "errorClass": "internal_error"
        });
        assert_eq!(
            quota_disconnect_kind(&entry),
            QuotaDisconnectKind::ModuleDefect
        );
    }

    /// The class list is open and grows on the producer's side. A class this
    /// build has never seen must surface rather than be filed under "nothing to
    /// do" — the direction matters, because the quiet failure is the one that
    /// reproduces the blindness the field was added to remove.
    #[test]
    fn a_class_this_build_has_never_seen_still_surfaces() {
        let entry = serde_json::json!({
            "provider": "p", "error": "something new", "errorClass": "a_class_from_the_future"
        });
        assert_eq!(
            quota_disconnect_kind(&entry),
            QuotaDisconnectKind::UserFixable
        );
    }

    /// A producer predating the field must render exactly as it did before, or
    /// shipping this turns every disconnected provider on an older fleet into an
    /// alarm. Not vacuous: the entry really is disconnected, so it cannot pass by
    /// being mistaken for a healthy one.
    #[test]
    fn an_entry_with_no_class_is_not_counted_as_failing() {
        let entry = serde_json::json!({ "provider": "p", "error": "no session: x" });
        assert!(!quota_entry_is_connected(&entry));
        assert_eq!(quota_disconnect_kind(&entry), QuotaDisconnectKind::Inert);
    }

    #[test]
    fn quota_verbose_flag_is_parsed_and_documented() {
        let command = parse_command(
            "quota",
            &[OsString::from("anthropic"), OsString::from("--verbose")],
        )
        .unwrap();
        assert!(matches!(
            command,
            Command::Quota {
                provider_id: Some(provider_id),
                verbose: true,
            } if provider_id == "anthropic"
        ));
        assert!(QUOTA_HELP.contains("--verbose"));
    }
}
