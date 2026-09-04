#![forbid(unsafe_code)]

//! `ck` — the CortexKit operator CLI.
//!
//! This binary is the founding piece of the CortexKit umbrella command. The
//! daemon/module control domain ships first, and the argument parser is shaped as
//! a small `<domain> <verb>` dispatcher so future domains such as `ck vault ...`,
//! `ck quota ...`, and `ck account ...` can be added without reshaping the CLI.

#[path = "../setup/mod.rs"]
mod setup;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, IsTerminal, Read, Seek, SeekFrom},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subc_control::{CatalogEntry, ClientControlRequest, ClientControlResponse};
// The connection-file name embeds a per-user token. `ck` must derive it the same
// way the daemon does, so it imports the daemon's function rather than carrying
// a copy -- these two used to be byte-identical duplicates in different files,
// with nothing asserting they agreed.
use subc_core::bootstrap::user_connection_token;
use subc_core::{fleet_lint, read_frame, write_frame, Frame};
use subc_protocol::{BindIdentity, Flags, FrameType, Priority, RouteTarget};
use subc_transport::{authenticate_client, connection_file, ConnectionFileError, ConnectionInfo};
use tokio::{net::TcpStream, time};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DASHBOARD_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_FILE_NAME: &str = "subc-connection.json";
const TRIAGE_LOG_MAX_BYTES: u64 = 64 * 1024;
const TRIAGE_LOG_TAIL_LINES: usize = 20;
const EXPECTED_DAEMON_BINARY: &str = "ck-subc";
const PROD_CONNECTION_RELATIVE_PATH: &[&str] =
    &[".local", "share", "cortexkit", "run", CONNECTION_FILE_NAME];
const QUOTA_MODULE_ID: &str = "insula";
const CK_HARNESS: &str = "ck";

#[cfg(feature = "test-support")]
const CK_BUILD_SHAPE: &str = "test-support: on";
#[cfg(not(feature = "test-support"))]
const CK_BUILD_SHAPE: &str = "test-support: off";
// Keep this conservative until the per-module split has supplied a week of
// production baseline data; then calibrate whether every window minute is needed.
const FRAME_DROP_ALERT_REQUIRED_NONZERO_MINUTES: u64 = 10;

const TOP_HELP_BASE: &str = "ck — CortexKit operator CLI\n\nusage:\n  ck [--subc <connection-file>] [--json] <domain> [<verb>] [<args>]\n\ndomains:\n  setup     plan and apply the managed CortexKit installation\n  upgrade   plan managed component upgrades\n  module    supervised modules: list, status, stderr, terminals, restart, stop, start, rescan, release\n  routes    live consumers for one module or the whole daemon\n  provenance daemon-attested and module-declared build/process facts\n  health    one-line health for every supervised module\n  quota     AI-provider quota and usage windows\n  daemon    daemon version, uptime, connection info, offline triage, and CI lint";

const TOP_HELP_TAIL: &str = "flags:\n  --subc <file>   use a specific connection file (default: auto-discover)\n  --json          raw JSON output instead of tables\n  --verbose       include diagnostic detail and complete metrics\n\nrun 'ck <domain>' with no verb to see that domain's commands";

const DOMAIN_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const DOMAIN_PROBE_CACHE_FILE: &str = "domain-probes.json";

/// Names of installed module programs, derived from setup's component tables so
/// the dispatcher cannot drift when a component gains another supported target.
fn module_command_ids() -> BTreeMap<String, String> {
    let mut modules = BTreeMap::new();
    for component in setup::Component::ALL {
        if let Some(program) = setup::module_program(component) {
            let command = program.strip_prefix("ck-").unwrap_or(program);
            let module_id = component.module_id().unwrap_or(command);
            modules.insert(command.to_string(), module_id.to_string());
        }
    }
    for target in setup::AlphaTarget::ALL {
        for binary in setup::component_binaries_for_target(setup::Component::Core, target) {
            let command = binary.strip_prefix("ck-").unwrap_or(binary);
            modules
                .entry(command.to_string())
                .or_insert_with(|| command.to_string());
        }
    }
    modules
}

#[derive(Clone, Debug)]
struct ExternalDomainCandidate {
    name: String,
    path: PathBuf,
}

#[derive(Clone, Debug)]
struct ExternalDomain {
    name: String,
    headline: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct DomainProbeCache {
    entries: BTreeMap<String, DomainProbeCacheEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DomainProbeCacheEntry {
    size: u64,
    modified_secs: u64,
    modified_nanos: u32,
    headline: Option<String>,
}

/// Top-level help has one domain list. External commands have to opt in so a
/// module binary on PATH cannot accidentally become an operator command.
fn top_help() -> String {
    top_help_with_tail(TOP_HELP_TAIL)
}

fn top_help_with_tail(tail: &str) -> String {
    let external = discover_external_domains();
    let mut out = String::from(TOP_HELP_BASE);
    for domain in &external {
        out.push_str(&format!("\n  {:<9} {}", domain.name, domain.headline));
    }
    out.push_str("\n\n");
    out.push_str(tail);
    out
}

/// Finds the first executable for each `ck-<name>` PATH entry, matching the
/// executable that dispatch would run when more than one directory has a name.
fn external_domain_candidates() -> BTreeMap<String, ExternalDomainCandidate> {
    let Some(path_var) = env::var_os("PATH") else {
        return BTreeMap::new();
    };
    let mut candidates = BTreeMap::new();
    for directory in env::split_paths(&path_var) {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(name) = name.strip_prefix("ck-") else {
                continue;
            };
            let name = name.strip_suffix(".exe").unwrap_or(name);
            if name.is_empty() || candidates.contains_key(name) {
                continue;
            }
            let path = entry.path();
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(&path)
                    .map(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                    .unwrap_or(false)
            };
            #[cfg(not(unix))]
            let executable = fs::metadata(&path)
                .map(|metadata| metadata.is_file())
                .unwrap_or(false);
            if !executable {
                continue;
            }
            let absolute = fs::canonicalize(&path).unwrap_or_else(|_| {
                if path.is_absolute() {
                    path
                } else {
                    env::current_dir()
                        .map(|current| current.join(path))
                        .unwrap_or_else(|_| PathBuf::from(name))
                }
            });
            candidates.insert(
                name.to_string(),
                ExternalDomainCandidate {
                    name: name.to_string(),
                    path: absolute,
                },
            );
        }
    }
    candidates
}

fn discover_external_domains() -> Vec<ExternalDomain> {
    external_domain_candidates()
        .into_values()
        .filter(|candidate| !is_builtin_domain(&candidate.name))
        .filter_map(|candidate| {
            probe_external_domain(&candidate).map(|headline| ExternalDomain {
                name: candidate.name,
                headline,
            })
        })
        .collect()
}

fn probe_external_domain(candidate: &ExternalDomainCandidate) -> Option<String> {
    let Some(cache_path) = domain_probe_cache_path() else {
        return run_domain_probe(&candidate.path);
    };
    let mut cache = read_domain_probe_cache(&cache_path);
    let stamp = domain_probe_stamp(&candidate.path)?;
    let key = candidate.path.to_string_lossy().into_owned();
    if let Some(entry) = cache.entries.get(&key) {
        if entry.size == stamp.size
            && entry.modified_secs == stamp.modified_secs
            && entry.modified_nanos == stamp.modified_nanos
        {
            return entry.headline.clone();
        }
    }

    let headline = run_domain_probe(&candidate.path);
    cache.entries.insert(
        key,
        DomainProbeCacheEntry {
            size: stamp.size,
            modified_secs: stamp.modified_secs,
            modified_nanos: stamp.modified_nanos,
            headline: headline.clone(),
        },
    );
    let _ = write_domain_probe_cache(&cache_path, &cache);
    headline
}

fn domain_probe_stamp(path: &Path) -> Option<DomainProbeCacheEntry> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(DomainProbeCacheEntry {
        size: metadata.len(),
        modified_secs: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
        headline: None,
    })
}

fn run_domain_probe(path: &Path) -> Option<String> {
    let mut child = process::Command::new(path)
        .arg("--ck-domain")
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(stdout), &mut bytes);
        bytes
    });
    let deadline = Instant::now() + DOMAIN_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader.join().ok()?;
    let status = status?;
    if !status.success() {
        return None;
    }
    let output = String::from_utf8(output).ok()?;
    let mut lines = output.lines();
    let headline = lines.next()?.trim();
    if headline.is_empty() || lines.next().is_some() {
        return None;
    }
    Some(headline.to_string())
}

/// Beside the update cache, so a test or operator that relocates one
/// relocates both. `None` means no cache: probes run every time.
fn domain_probe_cache_path() -> Option<PathBuf> {
    if let Some(update_cache_path) =
        env::var_os("CK_UPDATE_CACHE_PATH").filter(|path| !path.is_empty())
    {
        let update_cache_path = PathBuf::from(update_cache_path);
        if let Some(parent) = update_cache_path.parent() {
            return Some(parent.join(DOMAIN_PROBE_CACHE_FILE));
        }
    }
    setup::cache_directory().map(|directory| directory.join(DOMAIN_PROBE_CACHE_FILE))
}

fn read_domain_probe_cache(path: &Path) -> DomainProbeCache {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_domain_probe_cache(path: &Path, cache: &DomainProbeCache) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("cache path {} has no parent", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!("tmp-{}", process::id()));
    let bytes = serde_json::to_vec(cache).map_err(|error| error.to_string())?;
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    Ok(())
}

const MODULE_HELP: &str = "ck module — inspect and control supervised modules\n\nusage: ck [--json] [--verbose] module <verb> [<args>]\n\nverbs:\n  ck module list            all modules with state and health\n  ck module status <id>     one module in detail
  ck module stderr <id>     retained stderr for a module (-n <count> to limit)\n  ck module terminals <id>  retained terminal exits for a module\n  ck module restart <id>    drain-restart a module\n    --now                   restart without waiting for in-flight requests\n    --drain-ms <n>          wait up to <n> ms for in-flight requests (this restart only)\n  ck module stop <id>       disable and stop a module (persists until start)\n  ck module start <id>      enable and spawn a module\n  ck module rescan          re-read subc.jsonc and reconcile the module set\n  ck module rescan --dry-run  show what a rescan would change, without changing it\n  ck module release <id>    forget a removed module's reserved id so another module may use it";

const ROUTES_HELP: &str = "ck routes — inspect live route consumers\n\nusage: ck [--json] routes [<module-id>]\n\n  ck routes          live consumers for every connected module\n  ck routes <id>     live consumers for one connected module";

const PROVENANCE_HELP: &str = "ck provenance — inspect source-tagged module provenance\n\nusage: ck [--json] [--verbose] provenance <module-id>\n\n  ck provenance <id>  daemon-attested process facts beside module declarations";

const QUOTA_HELP: &str = "ck quota - AI-provider quota and usage windows\n\nusage: ck [--json] quota [--verbose] [<provider-id>]\n\n  ck quota              connected providers and their usage windows\n  ck quota --verbose    all tracked providers, including unavailable ones\n  ck quota claude       one provider's windows and status in detail";

const HEALTH_HELP: &str = "ck health — module health\n\nusage: ck [--json] [--verbose] health [<module-id>]\n\n  ck health            one-line health for every supervised module (cached)\n  ck health <id>       fresh health.check probe with headline metrics\n  ck health <id> --verbose  fresh probe with the complete metrics tree";

const DAEMON_HELP: &str = "ck daemon — daemon version, uptime, connection info, offline triage, and CI lint\n\nusage:\n  ck [--json] [--verbose] daemon\n  ck [--json] daemon triage\n  ck daemon lint [<config>] [--verbose]\n\n  triage reads only the local run directory; it never contacts the daemon.\n  lint reads module manifests without connecting to the daemon.";

const SETUP_HELP: &str = "ck setup — plan managed CortexKit installation\n\nusage:\n  ck setup [aft|mc|insula|claustrum|synapse] [--with aft,mc,insula,claustrum,synapse] [--dry-run] [--verbose]\n  ck setup claustrum [--key-path <file>]\n  ck setup <aft|mc> --convert [--confirm]\n  ck setup --uninstall [--dry-run]\n\n  Bare setup installs core and offers optional components. --dry-run prints the\n  complete plan without changing anything. --convert is explicit and requires\n  --confirm before it can apply a conversion plan. --verbose includes plan outcomes\n  and download diagnostics when a network request fails.";

const UPGRADE_HELP: &str = "ck upgrade — plan managed component upgrades\n\nusage:\n  ck upgrade [--verbose]\n  ck upgrade --check [--verbose]\n\n  --check reports available updates without changing anything. --verbose includes\n  plan outcomes and execution evidence.";

#[tokio::main]
async fn main() {
    match run(env::args_os()).await {
        Ok(()) => match cleanup_replaced_windows_ck() {
            Ok(()) => process::exit(0),
            Err(error) => {
                eprintln!("{error}");
                process::exit(error.exit_code());
            }
        },
        Err(
            CkError::FleetLintExit { exit_code }
            | CkError::TriageExit { exit_code }
            | CkError::RenderedExit { exit_code },
        ) => process::exit(exit_code),
        Err(err) => {
            eprintln!("{err}");
            process::exit(err.exit_code());
        }
    }
}

/// A Windows self-update leaves `ck.exe.old` until a later, successful process
/// proves that the replacement starts normally. Unix replacement has no old-name
/// artifact because rename keeps the running process on its original inode.
#[cfg(windows)]
fn cleanup_replaced_windows_ck() -> Result<(), CkError> {
    let executable = env::current_exe().map_err(|error| {
        CkError::Message(format!(
            "could not locate ck for self-update cleanup: {error}"
        ))
    })?;
    setup::cleanup_replaced_windows_ck(&executable).map_err(CkError::Message)
}

#[cfg(not(windows))]
fn cleanup_replaced_windows_ck() -> Result<(), CkError> {
    Ok(())
}

async fn run(argv: impl IntoIterator<Item = OsString>) -> Result<(), CkError> {
    let args = parse_args(argv)?;

    if matches!(&args.command, Command::BuildShape) {
        println!("{CK_BUILD_SHAPE}");
        return Ok(());
    }

    // Help and external dispatch resolve without a daemon connection: help is
    // static text, and an external ck-<domain> tool discovers its own connection.
    if let Command::Help(text) = &args.command {
        println!("{text}");
        return Ok(());
    }
    if let Command::External { domain, tail } = &args.command {
        return dispatch_external(domain, tail);
    }
    if matches!(&args.command, Command::Dashboard) {
        return dashboard(&args).await;
    }
    if let Command::FleetLint { config, verbose } = &args.command {
        return fleet_lint_command(config.as_deref(), *verbose).await;
    }
    if let Command::Setup(request) = &args.command {
        return setup_command(&running_executable()?, request);
    }
    if let Command::Upgrade { check, verbose } = &args.command {
        return upgrade_command(
            &running_executable()?,
            args.subc.as_deref(),
            *check,
            *verbose,
        )
        .await;
    }
    if matches!(&args.command, Command::DaemonTriage) {
        return daemon_triage(args.subc.as_deref(), args.json);
    }

    let resolved = discover_connection_file(args.subc.as_deref())
        .map_err(|error| decorate_error(error, args.json, args.subc.as_deref()))?;
    let mut client = CkClient::connect(resolved)
        .await
        .map_err(|error| decorate_error(error, args.json, args.subc.as_deref()))?;

    let result = match args.command {
        Command::Dashboard => unreachable!("handled before connecting"),
        Command::Module(ModuleCommand::List) => {
            module_list(&mut client, args.json, args.verbose, args.subc.as_deref()).await
        }
        Command::Module(ModuleCommand::Status { module_id }) => {
            module_status(&mut client, &module_id, args.json, args.verbose).await
        }
        Command::Module(ModuleCommand::StderrTail {
            module_id,
            max_lines,
        }) => module_stderr_tail(&mut client, &module_id, max_lines, args.json).await,
        Command::Module(ModuleCommand::Terminals { module_id }) => {
            module_terminals(&mut client, &module_id, args.json, args.verbose).await
        }
        Command::Module(ModuleCommand::Restart {
            module_id,
            drain_timeout_ms,
        }) => module_restart(&mut client, &module_id, drain_timeout_ms, args.json).await,
        Command::Module(ModuleCommand::Rescan { preview }) => {
            module_rescan(&mut client, args.json, preview).await
        }
        Command::Module(ModuleCommand::ReleaseReserved { module_id }) => {
            module_release_reserved(&mut client, &module_id, args.json).await
        }
        Command::Module(ModuleCommand::Stop { module_id }) => {
            module_set_enabled(&mut client, &module_id, false, args.json).await
        }
        Command::Module(ModuleCommand::Start { module_id }) => {
            module_set_enabled(&mut client, &module_id, true, args.json).await
        }
        Command::Routes { module_id } => {
            supervisor_routes(&mut client, module_id.as_deref(), args.json).await
        }
        Command::Provenance { module_id } => {
            provenance(&mut client, &module_id, args.json, args.verbose).await
        }
        Command::Health => health(&mut client, args.json, args.verbose, args.subc.as_deref()).await,
        Command::HealthDetail { module_id } => {
            health_detail(&mut client, &module_id, args.json, args.verbose).await
        }
        Command::Daemon => daemon(&mut client, args.json, args.verbose).await,
        Command::DaemonTriage => unreachable!("handled before connecting"),
        Command::Quota {
            provider_id,
            verbose,
        } => {
            quota(
                &mut client,
                provider_id.as_deref(),
                args.json,
                verbose,
                args.subc.as_deref(),
            )
            .await
        }
        Command::FleetLint { .. }
        | Command::Setup(_)
        | Command::Upgrade { .. }
        | Command::Help(_)
        | Command::External { .. }
        | Command::BuildShape => unreachable!("handled before connect"),
    };
    result.map_err(|error| decorate_error(error, args.json, args.subc.as_deref()))
}

/// Data collected by the bare-command probe. The module list supplies process
/// state, while the health response is the same stored health surface rendered by
/// `ck health`.
struct DashboardSnapshot {
    daemon_ver: String,
    pid: u32,
    path: PathBuf,
    describe: Value,
    modules: Value,
    health: Value,
}

struct DashboardUpdateLines {
    summary: String,
    cache_detail: Option<String>,
}

async fn fleet_lint_command(config: Option<&Path>, verbose: bool) -> Result<(), CkError> {
    let config = config
        .map(PathBuf::from)
        .unwrap_or_else(subc_core::daemon_config::default_config_path);
    let report = fleet_lint::lint(&config, verbose)
        .await
        .map_err(|error| CkError::FleetLintConfig(error.to_string()))?;
    println!("{}", report.render());
    if report.outcome == fleet_lint::LintOutcome::Clean {
        Ok(())
    } else {
        Err(CkError::FleetLintExit {
            exit_code: report.outcome.exit_code(),
        })
    }
}

async fn dashboard(args: &CkArgs) -> Result<(), CkError> {
    // Start release refresh beside the dashboard probe. `bare_update_line` uses
    // a fixed 800 ms deadline, so waiting for its output cannot add unbounded latency.
    let update_task = tokio::spawn(bare_update_line());
    let result = match discover_connection_file(args.subc.as_deref()) {
        Ok(resolved) => {
            let path = resolved.path.clone();
            match time::timeout(DASHBOARD_PROBE_TIMEOUT, dashboard_probe(resolved)).await {
                Ok(result) => result,
                Err(_) => Err(CkError::Connection {
                    path,
                    source: format!("dashboard probe timed out after {DASHBOARD_PROBE_TIMEOUT:?}"),
                }),
            }
        }
        Err(error) => Err(error),
    };
    let update = update_task.await.unwrap_or_else(|_| DashboardUpdateLines {
        summary: "updates: unknown (could not reach cortexkit.io)".to_string(),
        cache_detail: None,
    });

    match result {
        Ok(snapshot) => print_dashboard(&args.program, &snapshot, &update, args.verbose),
        Err(error) => print_degraded_dashboard(&args.program, &error, &update, args.verbose),
    }
    Ok(())
}

async fn bare_update_line() -> DashboardUpdateLines {
    let cache = setup::UpdateCache::from_environment();
    let source = setup::IndexReleaseSource::from_environment(setup::BARE_REFRESH_BUDGET);
    let installed = setup::dashboard_installed_binaries().unwrap_or_default();
    let rendered = setup::dashboard_update(&cache, &source, &installed)
        .await
        .render();
    let (without_cache, cache_detail) = rendered
        .strip_suffix(')')
        .and_then(|line| line.rsplit_once(" (cache "))
        .map(|(summary, cache)| (summary.to_string(), Some(cache.to_string())))
        .unwrap_or_else(|| (rendered.clone(), None));
    let summary = if without_cache == "updates: current" {
        "updates: none".to_string()
    } else if without_cache.starts_with("updates: not checked") {
        "updates: unknown (could not reach cortexkit.io)".to_string()
    } else if without_cache == "updates: index stale" {
        without_cache
    } else {
        format!("{without_cache} (run ck upgrade)")
    };
    DashboardUpdateLines {
        summary,
        cache_detail,
    }
}

async fn dashboard_probe(resolved: ResolvedConnection) -> Result<DashboardSnapshot, CkError> {
    let mut client = CkClient::connect(resolved).await?;
    let path = client.path.clone();
    let describe = client
        .rpc_value(ClientControlRequest::ServerDescribe {})
        .await
        .map_err(|error| dashboard_connection_error(&path, error))?;
    let modules = supervisor_list(&mut client)
        .await
        .map_err(|error| dashboard_connection_error(&path, error))?;
    let health = supervisor_health(&mut client)
        .await
        .map_err(|error| dashboard_connection_error(&path, error))?;
    Ok(DashboardSnapshot {
        daemon_ver: client.info.daemon_ver.clone(),
        pid: client.info.pid,
        path: client.path.clone(),
        describe,
        modules,
        health,
    })
}

fn print_dashboard(
    program: &Path,
    snapshot: &DashboardSnapshot,
    update: &DashboardUpdateLines,
    verbose: bool,
) {
    let clients = display_field(&snapshot.describe, "connected_clients");
    let uptime = connection_file_age(&snapshot.path)
        .map(format_duration)
        .unwrap_or_else(|| "-".to_string());
    println!(
        "ck {} · daemon running (pid {}, up {uptime}, {clients} clients)",
        env!("CARGO_PKG_VERSION"),
        snapshot.pid
    );
    print_dashboard_module_summary(&snapshot.modules, &snapshot.health);
    if let Some(alerts) = dashboard_alerts_line(&snapshot.describe) {
        println!("{alerts}");
    }
    println!("{}", update.summary);

    if verbose {
        print_dashboard_identity(program);
        println!("daemon version: {}", snapshot.daemon_ver);
        if let Some(cache_detail) = &update.cache_detail {
            println!("update cache: {cache_detail}");
        }
    }
    let cli_sha = env!("SUBC_BUILD_GIT_SHA");
    if snapshot
        .describe
        .get("build_git_sha")
        .and_then(Value::as_str)
        .is_some_and(|daemon_sha| daemon_sha != cli_sha)
    {
        print_build_skew(&snapshot.describe);
    }

    print_static_domains();
    println!("run `ck <command>` for its verbs, `ck --help` for everything");
}

fn print_degraded_dashboard(
    program: &Path,
    error: &CkError,
    update: &DashboardUpdateLines,
    verbose: bool,
) {
    println!(
        "ck {} · daemon stopped ({})",
        env!("CARGO_PKG_VERSION"),
        dashboard_error_text(error)
    );
    println!("{}", update.summary);
    if verbose {
        print_dashboard_identity(program);
        if let Some(cache_detail) = &update.cache_detail {
            println!("update cache: {cache_detail}");
        }
    }
    print_static_domains();
    print_help_footer(&[
        "Check the connection file path above, then run `ck daemon --subc <connection-file>`",
    ]);
}

fn dashboard_connection_error(path: &Path, error: CkError) -> CkError {
    CkError::Connection {
        path: path.to_path_buf(),
        source: error.to_string(),
    }
}

fn dashboard_error_text(error: &CkError) -> String {
    match error {
        CkError::Discovery { .. } | CkError::Connection { .. } => error.to_string(),
        other => format!("subc daemon did not answer: {other}"),
    }
}

fn print_dashboard_identity(program: &Path) {
    let (path, real_path) = executable_identity(program);
    let path = display_home_path(&path);
    match real_path {
        Some(real_path) => println!("bin: {path} ({})", display_home_path(&real_path)),
        None => println!("bin: {path}"),
    }
}

fn print_dashboard_module_summary(modules: &Value, health: &Value) {
    let module_entries = modules_array(modules);
    let health_entries = modules_array(health);
    if module_entries.is_empty() {
        println!("modules: none");
        return;
    }

    let summaries = module_entries
        .iter()
        .map(|module| {
            let module_id = display_field(module, "module_id");
            if dashboard_module_is_warming(health_entries, module) {
                return format!("{module_id} warming");
            }
            let status = human_health_status(&dashboard_health_status(health_entries, module));
            let detail = health_entries
                .iter()
                .find(|entry| display_field(entry, "module_id") == module_id)
                .and_then(health_operator_detail);
            if status != "ok" {
                if let Some(detail) = detail {
                    return format!("{module_id} {status} ({detail})");
                }
            }
            format!("{module_id} {status}")
        })
        .collect::<Vec<_>>();
    println!("modules: {}", summaries.join(" · "));
}

/// Alerts that do not already appear in the inline module summary stay on a
/// separate line. Omitting an empty line keeps a healthy dashboard scannable.
fn dashboard_alerts_line(describe: &Value) -> Option<String> {
    dashboard_frame_drop_alert(describe).map(|alert| format!("alerts: {alert}"))
}

fn dashboard_frame_drop_alert(describe: &Value) -> Option<String> {
    let counters = describe.get("counters")?.as_object()?;
    let nonzero_minutes = counters
        .get("module_frames_dropped_no_route_nonzero_minutes_last_10m")?
        .as_u64()?;
    if nonzero_minutes != FRAME_DROP_ALERT_REQUIRED_NONZERO_MINUTES {
        return None;
    }
    let drops = counters
        .get("module_frames_dropped_no_route_last_10m")?
        .as_u64()?;
    if drops == 0 {
        return None;
    }
    let top_module = counters
        .get("module_frames_dropped_no_route_by_module")
        .and_then(Value::as_object)
        .and_then(|modules| {
            modules
                .iter()
                .filter_map(|(module_id, count)| count.as_u64().map(|count| (module_id, count)))
                .max_by(|(left_id, left_count), (right_id, right_count)| {
                    left_count
                        .cmp(right_count)
                        .then_with(|| right_id.cmp(left_id))
                })
                .map(|(module_id, _)| module_id)
        })
        .map(String::as_str)
        .unwrap_or("unknown");
    Some(format!("frame drops ({drops} in 10m, top: {top_module})"))
}

fn dashboard_health_status(health_entries: &[Value], module: &Value) -> String {
    let module_id = display_field(module, "module_id");
    health_entries
        .iter()
        .find(|entry| display_field(entry, "module_id") == module_id)
        .map(|entry| display_field(entry, "status"))
        .unwrap_or_else(|| "unknown".to_string())
}

/// `last_probe_ms: None` means the supervisor has never probed this module, so
/// an unknown status is expected while its first probe is pending. A present
/// stamp proves the probe ran; an unknown result after that is an alarm.
fn dashboard_module_is_warming(health_entries: &[Value], module: &Value) -> bool {
    if dashboard_health_status(health_entries, module) != "unknown" {
        return false;
    }
    let module_id = display_field(module, "module_id");
    // The two surfaces spell "never probed" differently on the wire: the
    // health entry omits the key, the supervisor entry serializes it as
    // `null`. `Value::get` returns `Some(Null)` for the latter, so a bare
    // `is_none()` read every registered module as probed — which is how the
    // dashboard alerted on three modules 38 s after a fresh install.
    let probed = |value: &Value| value.get("last_probe_ms").is_some_and(|v| !v.is_null());
    let health_probed = health_entries
        .iter()
        .find(|entry| display_field(entry, "module_id") == module_id)
        .is_some_and(probed);
    !(health_probed || probed(module))
}

fn print_static_domains() {
    const BUILTIN_DOMAINS: [&str; 7] = [
        "setup", "upgrade", "module", "health", "routes", "quota", "daemon",
    ];
    let mut domains = BUILTIN_DOMAINS
        .iter()
        .map(|domain| (*domain).to_string())
        .collect::<Vec<_>>();
    for external in discover_external_domains() {
        if !domains.iter().any(|domain| domain == &external.name) {
            domains.push(external.name);
        }
    }
    println!("\ncommands: {}", domains.join(" · "));
}

/// Lifecycle mutations identify the executable inode, not argv[0]: a shell
/// invocation through PATH commonly supplies only the bare display name `ck`.
fn running_executable() -> Result<PathBuf, CkError> {
    let path = env::current_exe()
        .map_err(|error| CkError::Message(format!("could not identify the running ck: {error}")))?;
    fs::canonicalize(&path).map_err(|error| {
        CkError::Message(format!(
            "could not canonicalize running ck {}: {error}",
            path.display()
        ))
    })
}

fn executable_identity(argv0: &Path) -> (PathBuf, Option<PathBuf>) {
    let candidate = locate_executable(argv0).or_else(|| env::current_exe().ok());
    let Some(candidate) = candidate else {
        return (argv0.to_path_buf(), None);
    };
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(candidate))
            .unwrap_or_else(|_| argv0.to_path_buf())
    };
    let is_symlink = fs::symlink_metadata(&absolute)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false);
    let real = fs::canonicalize(&absolute).ok();
    let real_path = real.filter(|real| is_symlink && real != &absolute);
    (absolute, real_path)
}

fn locate_executable(argv0: &Path) -> Option<PathBuf> {
    if argv0.is_absolute() || argv0.components().count() > 1 || argv0.exists() {
        return Some(argv0.to_path_buf());
    }
    let path_var = env::var_os("PATH")?;
    for directory in env::split_paths(&path_var) {
        let candidate = directory.join(argv0);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn display_home_path(path: &Path) -> String {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    if path == home {
        "~".to_string()
    } else if let Ok(relative) = path.strip_prefix(&home) {
        format!(
            "~{}",
            if relative.as_os_str().is_empty() {
                String::new()
            } else {
                format!("/{}", relative.display())
            }
        )
    } else {
        path.display().to_string()
    }
}

/// Dispatch only a binary that completed the domain handshake. The handshake
/// result is cached, but a changed executable gets a new stamp and is probed
/// again before it can receive operator input.
fn dispatch_external(domain: &str, tail: &[OsString]) -> Result<(), CkError> {
    let module_ids = module_command_ids();
    let candidate = external_domain_candidates().remove(domain);
    let Some(candidate) = candidate else {
        return if let Some(module_id) = module_ids.get(domain) {
            Err(CkError::ModuleNotCommand {
                command: domain.to_string(),
                module_id: module_id.clone(),
            })
        } else {
            Err(CkError::Message(format!(
                "unknown command '{domain}'. Run ck --help."
            )))
        };
    };
    if probe_external_domain(&candidate).is_some() {
        let status = process::Command::new(&candidate.path)
            .args(tail)
            .status()
            .map_err(|error| {
                CkError::Message(format!(
                    "failed to run {}: {error}",
                    candidate.path.display()
                ))
            })?;
        process::exit(status.code().unwrap_or(1));
    }
    if let Some(module_id) = module_ids.get(domain) {
        return Err(CkError::ModuleNotCommand {
            command: domain.to_string(),
            module_id: module_id.clone(),
        });
    }
    Err(CkError::Message(format!(
        "unknown command '{domain}'. Run ck --help."
    )))
}

struct CkArgs {
    program: PathBuf,
    subc: Option<PathBuf>,
    json: bool,
    verbose: bool,
    command: Command,
}

enum Command {
    /// Prints the compile-time test-support state so integration tests reject a
    /// binary that cannot verify their fixture-signed release index.
    BuildShape,
    Dashboard,
    Setup(setup::SetupRequest),
    Upgrade {
        check: bool,
        verbose: bool,
    },
    Module(ModuleCommand),
    Routes {
        module_id: Option<String>,
    },
    Provenance {
        module_id: String,
    },
    Health,
    HealthDetail {
        module_id: String,
    },
    Daemon,
    DaemonTriage,
    Quota {
        provider_id: Option<String>,
        verbose: bool,
    },
    FleetLint {
        config: Option<PathBuf>,
        verbose: bool,
    },
    /// Explicit help request (`ck <domain>` with no verb, `ck help …`,
    /// `-h/--help`, or bare `ck --json`): prints to stdout and exits 0 without
    /// touching the daemon.
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
        /// `Some(ms)` overrides the module's configured drain budget for this
        /// one restart; `Some(0)` (from --now) skips the drain entirely.
        drain_timeout_ms: Option<u64>,
    },
    Rescan {
        preview: bool,
    },
    ReleaseReserved {
        module_id: String,
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
    Terminals {
        module_id: String,
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

async fn module_list(
    client: &mut CkClient,
    json_output: bool,
    verbose: bool,
    subc: Option<&Path>,
) -> Result<(), CkError> {
    let value = supervisor_list(client).await?;
    if json_output {
        print_json(&value)?;
    } else {
        let modules = modules_array(&value);
        if modules.is_empty() {
            println!("no supervised modules");
            let footer = [next_step(
                "ck module rescan",
                "to reconcile configured modules",
                subc,
            )];
            print_help_footer(&footer);
        } else {
            print_module_table(modules, verbose);
        }
    }
    Ok(())
}

async fn module_status(
    client: &mut CkClient,
    module_id: &str,
    json_output: bool,
    verbose: bool,
) -> Result<(), CkError> {
    let list = supervisor_list(client).await?;
    let Some(module) = find_module(&list, module_id).cloned() else {
        return if json_output {
            Err(CkError::Rejected(format!(
                "module_id '{module_id}' is not supervised"
            )))
        } else {
            Err(CkError::Rejected(format!(
                "no module named '{module_id}'. Run ck module list."
            )))
        };
    };
    let health = supervisor_health(client).await?;
    let health_entry = find_module(&health, module_id).cloned();
    let describe = client
        .rpc_value(ClientControlRequest::ServerDescribe {})
        .await?;
    let frame_drops = module_frame_drop_count(&describe, module_id);

    if json_output {
        print_json(&json!({ "module": module, "health": health_entry }))?;
    } else {
        let provenance = client
            .rpc_value(ClientControlRequest::SupervisorProvenance {
                module_id: Some(module_id.to_string()),
            })
            .await?;
        let observed = modules_array(&provenance)
            .first()
            .and_then(|entry| entry.get("daemon_observed"));
        print_status_table(
            &module,
            health_entry.as_ref(),
            frame_drops,
            observed,
            verbose,
        );
    }
    Ok(())
}

/// Read `-n <count>` from a verb's own tail.
///
/// Scoped to the verb rather than the global argument set, like `--dry-run` on
/// rescan, so it cannot silently apply somewhere else.
/// `--now` and `--drain-ms <N>` on `ck module restart`: one restart's drain
/// override. Returns `None` when neither flag is present so the wire request
/// omits the field entirely (older daemons reject unknown channel-0 fields).
/// `--now` is exactly `--drain-ms 0`; passing both is refused as ambiguous
/// rather than silently picking one.
fn parse_drain_override(tail: &[std::ffi::OsString]) -> Result<Option<u64>, CkError> {
    let now = tail.iter().any(|t| t == "--now");
    let mut drain_ms: Option<u64> = None;
    let mut iter = tail.iter();
    while let Some(token) = iter.next() {
        if token == "--drain-ms" {
            let value = iter.next().ok_or_else(|| {
                CkError::Usage(format!(
                    "--drain-ms needs a millisecond count\n\n{MODULE_HELP}"
                ))
            })?;
            let parsed = value
                .to_str()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| {
                    CkError::Usage(format!(
                        "--drain-ms '{}' is not a millisecond count\n\n{MODULE_HELP}",
                        value.to_string_lossy()
                    ))
                })?;
            drain_ms = Some(parsed);
        }
    }
    match (now, drain_ms) {
        (true, Some(_)) => Err(CkError::Usage(format!(
            "--now and --drain-ms are two answers to the same question; pass one\n\n{MODULE_HELP}"
        ))),
        (true, None) => Ok(Some(0)),
        (false, value) => Ok(value),
    }
}

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
    if let Some(hint) = stderr_truncation_hint(&response) {
        println!("{hint}");
    }
    Ok(())
}

fn stderr_truncation_hint(response: &Value) -> Option<String> {
    let dropped = response
        .get("dropped_lines")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if dropped == 0 {
        return None;
    }
    let shown = response
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry.get("kind").and_then(Value::as_str) == Some("line"))
                .count() as u64
        })
        .unwrap_or(0);
    let total = response
        .get("total_lines")
        .or_else(|| response.get("line_count"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| shown.saturating_add(dropped));
    Some(format!(
        "(showing {shown} of {total} lines · dropped {dropped} — use -n <count> for more)"
    ))
}

async fn module_terminals(
    client: &mut CkClient,
    module_id: &str,
    json_output: bool,
    verbose: bool,
) -> Result<(), CkError> {
    let response = client
        .rpc_value(ClientControlRequest::SupervisorTerminals {
            module_id: module_id.to_string(),
        })
        .await?;

    if json_output {
        print_json(&response)?;
        return Ok(());
    }

    let daemon_started_at_ms = response.get("daemon_started_at_ms").and_then(Value::as_u64);
    let dropped = response.get("dropped").and_then(Value::as_u64).unwrap_or(0);
    let entries = response
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        let started = daemon_started_at_ms
            .map(format_age_from_epoch_ms_now)
            .unwrap_or_else(|| "an unknown time ago".to_string());
        println!("no exits recorded since the daemon started ({started})");
    } else {
        let rows = entries
            .iter()
            .map(|entry| {
                let when = entry
                    .get("at_ms")
                    .and_then(Value::as_u64)
                    .map(format_age_from_epoch_ms_now)
                    .unwrap_or_else(|| "-".to_string());
                let exit = format_exit(
                    entry.get("exit_signal").and_then(Value::as_i64),
                    entry.get("exit_code").and_then(Value::as_i64),
                )
                .unwrap_or_else(|| "unknown".to_string());
                let disposition = entry
                    .get("disposition")
                    .and_then(Value::as_str)
                    .map(humanize_identifier)
                    .unwrap_or_else(|| "unknown".to_string());
                vec![when, exit, disposition]
            })
            .collect();
        print_table(&["when", "exit", "disposition"], rows);
    }
    if verbose && dropped > 0 {
        println!("{dropped} earlier exits were dropped from the daemon's retained history");
    }
    Ok(())
}

async fn module_restart(
    client: &mut CkClient,
    module_id: &str,
    drain_timeout_ms: Option<u64>,
    json_output: bool,
) -> Result<(), CkError> {
    let ack = client
        .rpc_value(ClientControlRequest::SupervisorRestart {
            module_id: module_id.to_string(),
            drain_timeout_ms,
        })
        .await?;
    print_ack_with_state(client, module_id, ack, "restart", json_output).await?;
    if !json_output {
        // The ack means INITIATED, not completed: the daemon drains and respawns
        // asynchronously precisely so a caller whose own tool lane rides this
        // module can settle instead of deadlocking the drain. The state column
        // above is therefore usually "restarting"; completion is a status read.
        println!("restart initiated; verify: ck module status {module_id}");
    }
    Ok(())
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

async fn module_release_reserved(
    client: &mut CkClient,
    module_id: &str,
    json_output: bool,
) -> Result<(), CkError> {
    let ack = client
        .rpc_value(ClientControlRequest::SupervisorReleaseReserved {
            module_id: module_id.to_string(),
        })
        .await?;
    print_ack_with_state(client, module_id, ack, "release", json_output).await
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

async fn supervisor_routes(
    client: &mut CkClient,
    module_id: Option<&str>,
    json_output: bool,
) -> Result<(), CkError> {
    let response = client
        .rpc_value(ClientControlRequest::SupervisorRoutes {
            module_id: module_id.map(str::to_owned),
        })
        .await?;
    if json_output {
        print_json(&response)?;
        return Ok(());
    }

    let mut rows = Vec::new();
    for module in modules_array(&response) {
        let module_id = display_field(module, "module_id");
        for route in module
            .get("routes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let consumer = match route.get("consumer").and_then(Value::as_object) {
                Some(consumer)
                    if consumer.get("kind").and_then(Value::as_str) == Some("reserved") =>
                {
                    consumer
                        .get("module_id")
                        .and_then(Value::as_str)
                        .unwrap_or("reserved")
                        .to_string()
                }
                Some(consumer) => format!(
                    "direct ({})",
                    consumer
                        .get("connection_id")
                        .and_then(Value::as_u64)
                        .map(|id| format!("connection {id}"))
                        .unwrap_or_else(|| "connection unknown".to_string())
                ),
                None => "direct (connection unknown)".to_string(),
            };
            let age = route
                .get("age_ms")
                .and_then(Value::as_u64)
                .map(|age| format_duration(Duration::from_millis(age)))
                .unwrap_or_else(|| "-".to_string());
            let state = if route
                .get("draining")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                match route.get("drain_reason").and_then(Value::as_str) {
                    Some(reason) => format!("draining ({})", humanize_identifier(reason)),
                    None => "draining".to_string(),
                }
            } else {
                "live".to_string()
            };
            rows.push(vec![module_id.clone(), consumer, age, state]);
        }
    }

    if rows.is_empty() {
        println!("no live routes");
    } else {
        print_table(&["module", "consumer", "age", "state"], rows);
    }
    Ok(())
}

async fn provenance(
    client: &mut CkClient,
    module_id: &str,
    json_output: bool,
    verbose: bool,
) -> Result<(), CkError> {
    let response = client
        .rpc_value(ClientControlRequest::SupervisorProvenance {
            module_id: Some(module_id.to_string()),
        })
        .await?;
    if json_output {
        print_json(&response)?;
        return Ok(());
    }

    let daemon = response
        .get("daemon")
        .ok_or_else(|| CkError::Message("provenance response omitted daemon".to_string()))?;
    let module = modules_array(&response)
        .first()
        .ok_or_else(|| CkError::Message("provenance response omitted module".to_string()))?;

    println!("Daemon build");
    let daemon_build = daemon.get("daemon_build").unwrap_or(&Value::Null);
    println!(
        "  commit: {}",
        provenance_value(daemon_build.get("build_git_sha"))
    );
    println!(
        "  lock digest: {}",
        provenance_value(daemon_build.get("build_lock_digest"))
    );
    println!("Daemon observed");
    let daemon_observed = daemon.get("daemon_observed").unwrap_or(&Value::Null);
    println!("  pid: {}", provenance_value(daemon_observed.get("pid")));
    println!(
        "  started {}",
        provenance_timestamp(daemon_observed.get("started_at_ms"))
    );
    println!(
        "  running image {}",
        provenance_image(daemon_observed.get("running_image"))
    );
    if verbose {
        print_running_image_evidence(daemon_observed.get("running_image"), 2);
    }

    println!("Module: {}", provenance_value(module.get("module_id")));
    println!("Module declared");
    match module.get("module_declared") {
        Some(declared) if declared.get("status").and_then(Value::as_str) == Some("reported") => {
            let build = declared.get("build").unwrap_or(&Value::Null);
            let commit = build.get("build_git_sha");
            println!("  commit: {}", provenance_value(commit));
            if commit
                .and_then(Value::as_str)
                .is_some_and(|value| value.ends_with("-dirty"))
            {
                println!("  status: commit match only");
            }
            let lock = build.get("build_lock_digest");
            println!("  lock digest: {}", provenance_value(lock));
            if commit.is_none() && lock.is_some() {
                println!("  status: change-detectable; commit identity unavailable");
            }
            println!(
                "  wire crate version: {}",
                provenance_value(build.get("wire_crate_version"))
            );
            println!(
                "  store schema version: {}",
                provenance_value(build.get("store_schema_version"))
            );
        }
        _ => println!("  unverifiable"),
    }

    println!("Daemon observed");
    let observed = module.get("daemon_observed").unwrap_or(&Value::Null);
    println!("  pid: {}", provenance_value(observed.get("pid")));
    println!(
        "  started {}",
        provenance_timestamp(observed.get("spawned_at_ms"))
    );
    println!(
        "  spawned from: {}",
        provenance_value(observed.get("spawned_from"))
    );
    println!(
        "  running image {}",
        provenance_image(observed.get("running_image"))
    );
    if verbose {
        print_running_image_evidence(observed.get("running_image"), 2);
    }
    Ok(())
}

fn provenance_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value
            .bytes()
            .map(|byte| match byte {
                0x20..=0x7e => (byte as char).to_string(),
                _ => format!(r"\x{byte:02x}"),
            })
            .collect(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Bool(true)) => "enabled".to_string(),
        Some(Value::Bool(false)) => "disabled".to_string(),
        _ => "none".to_string(),
    }
}

fn provenance_image(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "is unknown".to_string();
    };
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match status {
        "match" => "matches the file it was spawned from".to_string(),
        "mismatch" => "differs from the file it was spawned from (running vs disk)".to_string(),
        "unavailable" => format!(
            "could not be compared with the file it was spawned from ({})",
            provenance_value(value.get("reason"))
        ),
        other => {
            let tag = provenance_value(Some(&Value::String(other.to_string())));
            let body = serde_json::to_string(value)
                .map(|body| provenance_value(Some(&Value::String(body))))
                .unwrap_or_else(|_| "none".to_string());
            format!("has unknown status ({tag}; body={body})")
        }
    }
}

fn print_running_image_evidence(image: Option<&Value>, depth: usize) {
    let Some(image) = image else {
        return;
    };
    for key in ["evidence", "running", "disk"] {
        if let Some(value) = image.get(key) {
            println!("{}{}:", "  ".repeat(depth), humanize_identifier(key));
            print_metrics_tree(value, depth + 1);
        }
    }
}

fn provenance_timestamp(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_u64)
        .map(format_age_from_epoch_ms_now)
        .unwrap_or_else(|| "unknown".to_string())
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

async fn health(
    client: &mut CkClient,
    json_output: bool,
    verbose: bool,
    subc: Option<&Path>,
) -> Result<(), CkError> {
    let value = supervisor_health(client).await?;
    if json_output {
        print_json(&value)?;
    } else {
        print_health_table(modules_array(&value), verbose, subc);
    }
    Ok(())
}

async fn health_detail(
    client: &mut CkClient,
    module_id: &str,
    json_output: bool,
    verbose: bool,
) -> Result<(), CkError> {
    if !json_output {
        let modules = supervisor_list(client).await?;
        if find_module(&modules, module_id).is_none() {
            return Err(CkError::Rejected(format!(
                "no module named '{module_id}'. Run ck module list."
            )));
        }
    }

    let value = client
        .rpc_value(ClientControlRequest::SupervisorHealthProbe {
            module_id: module_id.to_string(),
        })
        .await?;
    if json_output {
        print_json(&value)?;
        return Ok(());
    }
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .map(human_health_status)
        .unwrap_or_else(|| "unknown".to_string());
    println!("{module_id}: {status}");
    if let Some(detail) = health_operator_detail(&value) {
        println!("  {detail}");
    }
    match value.get("metrics") {
        Some(metrics) if !metrics.is_null() && verbose => print_metrics_tree(metrics, 1),
        Some(metrics) if !metrics.is_null() => print_headline_metrics(metrics, 1),
        _ => println!("  metrics: none"),
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
        Value::Array(items) if items.is_empty() => println!("{indent}{key}: none"),
        Value::Array(_) => {
            println!("{indent}{key}:");
            print_metrics_tree(child, depth + 1);
        }
        other => println!("{indent}{key}: {}", format_metric_value(key, other)),
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

fn inline_scalar_object(map: &serde_json::Map<String, Value>) -> Option<String> {
    if map.is_empty() {
        return Some("none".to_string());
    }
    let mut parts = Vec::with_capacity(map.len());
    for (key, value) in map {
        match value {
            Value::Object(_) | Value::Array(_) => return None,
            other => parts.push(format!("{key}={}", format_metric_value(key, other))),
        }
    }
    let line = parts.join(" · ");
    (line.chars().count() <= 88).then_some(line)
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => "none".to_string(),
        Value::String(value) if value.is_empty() => "none".to_string(),
        Value::String(value) => value.clone(),
        Value::Bool(true) => "enabled".to_string(),
        Value::Bool(false) => "disabled".to_string(),
        other => other.to_string(),
    }
}

fn print_headline_metrics(metrics: &Value, depth: usize) {
    let Some(map) = metrics.as_object() else {
        print_metrics_tree(metrics, depth);
        return;
    };
    let marked = map
        .get("headline")
        .or_else(|| map.get("_headline"))
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect::<Vec<_>>());
    let entries = match marked {
        Some(keys) => keys
            .into_iter()
            .filter_map(|key| map.get(key).map(|value| (key, value)))
            .collect::<Vec<_>>(),
        None => map
            .iter()
            .filter(|(_, value)| !value.is_array() && !value.is_object())
            .map(|(key, value)| (key.as_str(), value))
            .collect::<Vec<_>>(),
    };
    if entries.is_empty() {
        println!("{}metrics: none", "  ".repeat(depth));
        return;
    }
    for (key, value) in entries {
        print_metrics_entry(key, value, depth);
    }
}

fn format_metric_value(key: &str, value: &Value) -> String {
    match value {
        Value::Number(number) => {
            let normalized = key.to_ascii_lowercase();
            if normalized.contains("bytes") {
                return number
                    .as_u64()
                    .map(format_bytes)
                    .unwrap_or_else(|| number.to_string());
            }
            if normalized.ends_with("age_ms") || normalized.ends_with("agems") {
                return number
                    .as_u64()
                    .map(format_milliseconds)
                    .unwrap_or_else(|| number.to_string());
            }
            if normalized.ends_with("age_secs") || normalized.ends_with("agesecs") {
                return number
                    .as_u64()
                    .map(format_age_seconds)
                    .unwrap_or_else(|| number.to_string());
            }
            if normalized.ends_with("_ms") || normalized.ends_with("ms") {
                return number.as_u64().map_or_else(
                    || number.to_string(),
                    |milliseconds| {
                        if milliseconds >= 100_000_000_000 {
                            format_age_from_epoch_ms_now(milliseconds)
                        } else {
                            format_milliseconds(milliseconds)
                        }
                    },
                );
            }
            number.to_string()
        }
        other => scalar_text(other),
    }
}

fn format_milliseconds(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds} ms")
    } else {
        format_duration(Duration::from_millis(milliseconds))
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_f64 = bytes as f64;
    if bytes_f64 >= GIB {
        format!("{:.1} GiB", bytes_f64 / GIB)
    } else if bytes_f64 >= MIB {
        format!("{:.1} MiB", bytes_f64 / MIB)
    } else if bytes_f64 >= KIB {
        format!("{:.1} KiB", bytes_f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn daemon_triage(override_path: Option<&Path>, json_output: bool) -> Result<(), CkError> {
    let candidates = connection_file_candidates(override_path);
    let run_dir = candidates
        .first()
        .and_then(|path| path.parent())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let report = collect_daemon_triage(&candidates, &run_dir);
    if json_output {
        print_json(&report.json)
    } else {
        print_daemon_triage(&report);
        Ok(())
    }?;
    Err(CkError::TriageExit {
        exit_code: report.exit_code,
    })
}

struct TriageReport {
    json: Value,
    exit_code: i32,
    text: String,
}

fn collect_daemon_triage(candidates: &[PathBuf], run_dir: &Path) -> TriageReport {
    let mut start_locks = Vec::new();
    for candidate in candidates {
        let lock_path = triage_start_lock_path(candidate);
        start_locks.push(triage_file_fact(&lock_path));
    }

    let mut connection_candidates = Vec::new();
    let mut selected: Option<(PathBuf, Value, Option<u64>)> = None;
    for path in candidates {
        let mut fact = serde_json::Map::new();
        fact.insert("path".into(), json!(path.display().to_string()));
        let metadata = match fs::metadata(path) {
            Ok(metadata) => {
                fact.insert("status".into(), json!("present"));
                fact.insert("size_bytes".into(), json!(metadata.len()));
                fact.insert("mtime".into(), triage_mtime_fact(&metadata));
                Some(metadata)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fact.insert("status".into(), json!("absent"));
                fact.insert("finding".into(), json!("connection file absent"));
                None
            }
            Err(error) => {
                fact.insert("status".into(), json!("skipped"));
                fact.insert("skipped".into(), json!(format!("stat failed: {error}")));
                None
            }
        };
        if metadata.is_some() {
            match fs::read(path) {
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(value) => {
                        fact.insert("json".into(), json!("valid"));
                        let fields = triage_connection_fields(&value);
                        fact.insert("fields".into(), fields.clone());
                        let pid = value.get("pid").and_then(Value::as_u64);
                        if selected.is_none() {
                            selected = Some((path.clone(), value, pid));
                        }
                    }
                    Err(error) => {
                        fact.insert("json".into(), json!("invalid"));
                        fact.insert(
                            "finding".into(),
                            json!(format!("connection file JSON parse failure: {error}")),
                        );
                    }
                },
                Err(error) => {
                    fact.insert("status".into(), json!("skipped"));
                    fact.insert("skipped".into(), json!(format!("read failed: {error}")));
                }
            }
        }
        connection_candidates.push(Value::Object(fact));
    }

    let connection_fact = if let Some((path, value, pid)) = &selected {
        let fields = triage_connection_fields(value);
        let mut object = serde_json::Map::new();
        object.insert(
            "candidates".into(),
            Value::Array(connection_candidates.clone()),
        );
        object.insert("selected_path".into(), json!(path.display().to_string()));
        object.insert("fields".into(), fields);
        if let Some(port) = value
            .get("endpoints")
            .and_then(Value::as_array)
            .and_then(|endpoints| endpoints.first())
            .and_then(|endpoint| endpoint.get("port"))
        {
            object.insert("port".into(), port.clone());
        }
        if let Some(daemon_id) = value.get("daemon_id") {
            // The connection file stores daemon_id as a JSON byte array; render
            // it as hex so the identity is comparable against log lines and
            // other surfaces instead of appearing as a raw number list.
            let rendered = daemon_id
                .as_array()
                .map(|bytes| {
                    bytes
                        .iter()
                        .filter_map(Value::as_u64)
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                })
                .map(Value::from)
                .unwrap_or_else(|| daemon_id.clone());
            object.insert("daemon_id".into(), rendered);
        }
        if let Some(wire_version) = value.get("wire_version") {
            object.insert("wire_version".into(), wire_version.clone());
        }
        if let Some(pid) = pid {
            object.insert("pid".into(), json!(pid));
        }
        Value::Object(object)
    } else {
        json!({
            "candidates": connection_candidates.clone(),
            "status": "absent-or-unusable"
        })
    };

    let effective_run_dir = selected
        .as_ref()
        .and_then(|(path, _, _)| path.parent())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(run_dir);
    let pid = selected
        .as_ref()
        .and_then(|(_, _, pid)| *pid)
        .and_then(|pid| u32::try_from(pid).ok());
    let process_fact = triage_process_fact(pid);
    let log_path = effective_run_dir.join("subc.log");
    let log_fact = triage_log_fact(&log_path);
    let connection_present = selected.is_some()
        || connection_candidates
            .iter()
            .any(|candidate| candidate.get("status") == Some(&json!("present")));
    let parse_failure = connection_candidates
        .iter()
        .any(|candidate| candidate.get("json") == Some(&json!("invalid")));
    let fields_complete = selected.as_ref().is_some_and(|(_, value, _)| {
        triage_connection_fields(value)
            .get("port")
            .and_then(Value::as_object)
            .and_then(|value| value.get("present"))
            .and_then(Value::as_bool)
            == Some(true)
            && triage_connection_fields(value)
                .get("daemon_id")
                .and_then(Value::as_object)
                .and_then(|value| value.get("present"))
                .and_then(Value::as_bool)
                == Some(true)
            && triage_connection_fields(value)
                .get("wire_version")
                .and_then(Value::as_object)
                .and_then(|value| value.get("present"))
                .and_then(Value::as_bool)
                == Some(true)
    });
    let (verdict, reason, exit_code) = if !connection_present {
        ("daemon-appears-down", "no connection file", 2)
    } else if parse_failure && selected.is_none() {
        ("daemon-state-ambiguous", "connection file is malformed", 3)
    } else if !fields_complete {
        (
            "daemon-state-ambiguous",
            "connection file is missing required fields",
            3,
        )
    } else if process_fact.get("status") == Some(&json!("live")) {
        (
            "daemon-appears-live",
            "connection file is present and pid is alive",
            0,
        )
    } else if process_fact.get("status") == Some(&json!("dead")) {
        (
            "daemon-state-ambiguous",
            "fresh connection file conflicts with a dead pid",
            3,
        )
    } else {
        (
            "daemon-state-ambiguous",
            "pid liveness could not be established",
            3,
        )
    };
    let json_report = json!({
        "run_dir": effective_run_dir.display().to_string(),
        "start_lock": { "candidates": start_locks },
        "connection_file": connection_fact,
        "process_liveness": process_fact,
        "log_tail": log_fact,
        "verdict": { "status": verdict, "reason": reason }
    });
    let text = format!("run dir: {}\n", effective_run_dir.display());
    TriageReport {
        json: json_report,
        exit_code,
        text,
    }
}

fn triage_connection_fields(value: &Value) -> Value {
    let port = value
        .get("endpoints")
        .and_then(Value::as_array)
        .and_then(|endpoints| endpoints.first())
        .and_then(|endpoint| endpoint.get("port"));
    json!({
        "port": { "present": port.is_some(), "value": port.cloned().unwrap_or(Value::Bool(false)) },
        "daemon_id": { "present": value.get("daemon_id").is_some() },
        "wire_version": { "present": value.get("wire_version").is_some() }
    })
}

fn triage_file_fact(path: &Path) -> Value {
    match fs::metadata(path) {
        Ok(metadata) => json!({
            "path": path.display().to_string(),
            "status": "present",
            "size_bytes": metadata.len(),
            "mtime": triage_mtime_fact(&metadata)
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({
            "path": path.display().to_string(), "status": "absent", "finding": "start-lock absent"
        }),
        Err(error) => json!({
            "path": path.display().to_string(), "status": "skipped", "skipped": format!("stat failed: {error}")
        }),
    }
}

fn triage_mtime_fact(metadata: &fs::Metadata) -> Value {
    match metadata
        .modified()
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
    {
        Ok(age) => json!({ "status": "known", "age_seconds": age.as_secs() }),
        Err(error) => {
            json!({ "status": "skipped", "skipped": format!("mtime age unavailable: {error}") })
        }
    }
}

// bootstrap.rs:801-812 derives the advisory path as <connection-file>.start-lock
// in the connection file's parent; keep this disk-only copy aligned with that source.
fn triage_start_lock_path(connection_file_path: &Path) -> PathBuf {
    let file_name = connection_file_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| CONNECTION_FILE_NAME.into());
    let parent = connection_file_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent.join(format!("{file_name}.start-lock"))
}

fn triage_process_fact(pid: Option<u32>) -> Value {
    let Some(pid) = pid else {
        return json!({ "status": "skipped", "skipped": "no pid recovered from connection file or start-lock" });
    };
    // Platform split: `ps` is the honest probe on unix; Windows has no ps
    // executable (a PowerShell alias is not spawnable), so shelling it there
    // either fails to start or resolves to a Git-supplied ps.exe with
    // different flag semantics -- both misreport a live pid as dead/skipped.
    // tasklist ships with every Windows edition the fleet targets.
    #[cfg(not(windows))]
    let output = process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid=,comm="])
        .output();
    #[cfg(windows)]
    let output = process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    let Ok(output) = output else {
        return json!({ "pid": pid, "status": "skipped", "skipped": "process probe command could not be started" });
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // tasklist exits 0 with an INFO: prose line when the filter matches
    // nothing, so "no CSV row" is the not-running signal there; ps signals it
    // via exit status or empty output.
    #[cfg(windows)]
    let running = output.status.success() && stdout.starts_with('"');
    #[cfg(not(windows))]
    let running = output.status.success() && !stdout.is_empty();
    if !running {
        return json!({ "pid": pid, "status": "dead", "executable": { "status": "not-running" } });
    }
    #[cfg(windows)]
    let command = stdout
        .split('"')
        .nth(1)
        .unwrap_or(&stdout)
        .trim_end_matches(".exe")
        .to_string();
    #[cfg(not(windows))]
    let command = stdout;
    let executable = Path::new(command.split_whitespace().last().unwrap_or(&command))
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(&command);
    json!({
        "pid": pid,
        "status": "live",
        "executable": {
            "status": if executable == EXPECTED_DAEMON_BINARY { "match" } else { "mismatch" },
            "observed": executable,
            "expected": EXPECTED_DAEMON_BINARY
        }
    })
}

fn triage_log_fact(path: &Path) -> Value {
    let Ok(metadata) = fs::metadata(path) else {
        return if path.exists() {
            json!({ "path": path.display().to_string(), "status": "skipped", "skipped": "log metadata could not be read" })
        } else {
            json!({ "path": path.display().to_string(), "status": "absent", "finding": "log absent" })
        };
    };
    if metadata.len() == 0 {
        return json!({ "path": path.display().to_string(), "status": "empty", "finding": "log empty" });
    }
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "status": "skipped", "skipped": format!("log open failed: {error}") })
        }
    };
    let read_len = metadata.len().min(TRIAGE_LOG_MAX_BYTES);
    if file.seek(SeekFrom::End(-(read_len as i64))).is_err() {
        return json!({ "path": path.display().to_string(), "status": "skipped", "skipped": "log seek failed" });
    }
    let mut bytes = vec![0; read_len as usize];
    if let Err(error) = file.read_exact(&mut bytes) {
        return json!({ "path": path.display().to_string(), "status": "skipped", "skipped": format!("log read failed: {error}") });
    }
    let all = String::from_utf8_lossy(&bytes);
    let lines = all.lines().collect::<Vec<_>>();
    let tail = lines
        .iter()
        .rev()
        .take(TRIAGE_LOG_TAIL_LINES)
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    let truncated = metadata.len() > TRIAGE_LOG_MAX_BYTES;
    let mut fact = serde_json::Map::new();
    fact.insert("path".into(), json!(path.display().to_string()));
    fact.insert("status".into(), json!("present"));
    fact.insert("lines".into(), json!(tail));
    fact.insert("tail_lines".into(), json!(tail.len()));
    if truncated {
        fact.insert(
            "summary".into(),
            json!(format!(
                "last {} lines (read capped at {} bytes)",
                TRIAGE_LOG_TAIL_LINES, TRIAGE_LOG_MAX_BYTES
            )),
        );
    } else {
        fact.insert(
            "summary".into(),
            json!(format!("showing {} of {} lines", tail.len(), lines.len())),
        );
    }
    Value::Object(fact)
}

fn print_daemon_triage(report: &TriageReport) {
    let verdict = &report.json["verdict"];
    let status = triage_string(verdict.get("status"));
    let reason = triage_string(verdict.get("reason"));
    let pid = report.json["process_liveness"]
        .get("pid")
        .and_then(Value::as_u64);
    let verdict_line = match (status, pid) {
        ("daemon-appears-live", Some(pid)) => {
            format!("daemon appears live: connection file present, pid {pid} alive")
        }
        ("daemon-appears-down", _) => format!("daemon appears down: {reason}"),
        ("daemon-state-ambiguous", _) => format!("daemon state ambiguous: {reason}"),
        (_, _) => format!("{}: {reason}", humanize_identifier(status)),
    };
    println!("{verdict_line}");
    println!("{}", report.text.trim_end());

    let connection = &report.json["connection_file"];
    println!("start-lock:");
    for fact in report.json["start_lock"]["candidates"]
        .as_array()
        .into_iter()
        .flatten()
    {
        print_triage_file_fact(fact, false);
    }
    println!("connection-file:");
    for fact in connection["candidates"].as_array().into_iter().flatten() {
        print_triage_file_fact(fact, true);
    }
    if let Some(path) = connection.get("selected_path").and_then(Value::as_str) {
        println!("  selected: {path}");
        for field in ["port", "daemon_id", "wire_version"] {
            if let Some(value) = connection.get(field) {
                println!("    {field}: {}", scalar_text(value));
            }
        }
    }

    println!("process-liveness:");
    println!(
        "  {}",
        triage_process_line(&report.json["process_liveness"])
    );
    println!("log-tail:");
    let log = &report.json["log_tail"];
    println!(
        "  {}: {}",
        triage_string(log.get("path")),
        triage_string(log.get("status"))
    );
    if let Some(finding) = log.get("finding").and_then(Value::as_str) {
        println!("  finding: {finding}");
    }
    if let Some(skipped) = log.get("skipped").and_then(Value::as_str) {
        println!("  skipped: {skipped}");
    }
    if let Some(summary) = log.get("summary").and_then(Value::as_str) {
        println!("  {summary}");
    }
    if let Some(lines) = log.get("lines").and_then(Value::as_array) {
        for line in lines.iter().filter_map(Value::as_str) {
            println!("  {line}");
        }
    }
}

fn print_triage_file_fact(fact: &Value, include_connection_fields: bool) {
    println!(
        "  {}: {}",
        triage_string(fact.get("path")),
        triage_string(fact.get("status"))
    );
    if let Some(size) = fact.get("size_bytes").and_then(Value::as_u64) {
        println!("    size: {}", format_bytes(size));
    }
    if let Some(mtime) = fact.get("mtime") {
        let rendered = mtime
            .get("age_seconds")
            .and_then(Value::as_u64)
            .map(format_age_seconds)
            .or_else(|| {
                mtime
                    .get("skipped")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());
        println!("    modified: {rendered}");
    }
    if let Some(json_status) = fact.get("json").and_then(Value::as_str) {
        println!("    parses as JSON: {json_status}");
    }
    if include_connection_fields {
        if let Some(fields) = fact.get("fields").and_then(Value::as_object) {
            let rendered = ["daemon_id", "port", "wire_version"]
                .iter()
                .map(|field| {
                    let field_value = fields.get(*field);
                    let present = field_value
                        .and_then(|value| value.get("present"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let status = if present { "present" } else { "absent" };
                    let value = field_value
                        .and_then(|value| value.get("value"))
                        .filter(|value| !value.is_boolean())
                        .map(|value| format!(" ({})", scalar_text(value)))
                        .unwrap_or_default();
                    format!("{field} {status}{value}")
                })
                .collect::<Vec<_>>()
                .join(" · ");
            println!("    fields: {rendered}");
        }
    }
    if let Some(finding) = fact.get("finding").and_then(Value::as_str) {
        println!("    finding: {finding}");
    }
    if let Some(skipped) = fact.get("skipped").and_then(Value::as_str) {
        println!("    skipped: {skipped}");
    }
}

fn triage_process_line(process: &Value) -> String {
    let status = process
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let pid = process
        .get("pid")
        .and_then(Value::as_u64)
        .map(|pid| format!("pid {pid} "))
        .unwrap_or_default();
    let mut line = match status {
        "live" => format!("{pid}alive"),
        "dead" => format!("{pid}stopped"),
        other => format!("{pid}{}", humanize_identifier(other)),
    };
    if let Some(executable) = process.get("executable").and_then(Value::as_object) {
        let observed = executable
            .get("observed")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let expected = executable
            .get("expected")
            .and_then(Value::as_str)
            .unwrap_or(EXPECTED_DAEMON_BINARY);
        let agreement = match executable.get("status").and_then(Value::as_str) {
            Some("match") => "matches",
            Some("mismatch") => "differs from",
            Some("not-running") => "was expected to be",
            _ => "could not be compared with",
        };
        line.push_str(&format!("; executable {observed} {agreement} {expected}"));
    }
    if let Some(skipped) = process.get("skipped").and_then(Value::as_str) {
        line.push_str(&format!("; {skipped}"));
    }
    line
}

fn triage_string(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("-")
}

async fn daemon(client: &mut CkClient, json_output: bool, verbose: bool) -> Result<(), CkError> {
    let describe = client
        .rpc_value(ClientControlRequest::ServerDescribe {})
        .await?;
    if json_output {
        print_json(&describe)?;
        return Ok(());
    }

    let uptime = connection_file_age(&client.path)
        .map(format_duration)
        .unwrap_or_else(|| "-".to_string());
    let clients = display_field(&describe, "connected_clients");
    println!(
        "daemon {} · pid {} · up {uptime} · {clients} clients · {}",
        client.info.daemon_ver,
        client.info.pid,
        daemon_frame_drop_summary(&describe)
    );
    print_build_skew(&describe);
    if verbose {
        println!("protocol: {}", display_field(&describe, "protocol_ver"));
        if let Some(counters) = describe.get("counters").and_then(Value::as_object) {
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

fn daemon_frame_drop_summary(describe: &Value) -> String {
    let drops = describe
        .get("counters")
        .and_then(|counters| counters.get("module_frames_dropped_no_route_last_10m"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if drops == 0 {
        return "no frame drops in the last 10 minutes".to_string();
    }
    let top = describe
        .get("counters")
        .and_then(|counters| counters.get("module_frames_dropped_no_route_by_module"))
        .and_then(Value::as_object)
        .and_then(|modules| {
            modules
                .iter()
                .filter_map(|(module_id, count)| count.as_u64().map(|count| (module_id, count)))
                .max_by(|(left_id, left_count), (right_id, right_count)| {
                    left_count
                        .cmp(right_count)
                        .then_with(|| right_id.cmp(left_id))
                })
                .map(|(module_id, _)| module_id.as_str())
        })
        .unwrap_or("unknown");
    let noun = if drops == 1 { "drop" } else { "drops" };
    format!("{drops} frame {noun} in the last 10 minutes, top: {top}")
}

/// Compare the daemon's embedded build provenance against this CLI's own.
///
/// `daemon_ver` cannot catch a skewed pair: both binaries report the crate
/// version, which moves per release, so a daemon and a `ck` separated by many
/// wire-touching commits agree exactly when the disagreement matters. The
/// embedded commit moves per change. A daemon predating the field reports
/// nothing, and that prints as unverifiable rather than as a match -- absence
/// of the check must not read as the check passing.
fn print_build_skew(describe: &Value) {
    let cli_sha = env!("SUBC_BUILD_GIT_SHA");
    let daemon_sha = describe.get("build_git_sha").and_then(Value::as_str);
    match daemon_sha {
        None => println!(
            "build skew: daemon predates provenance reporting (CLI {cli_sha:.12}); unverifiable"
        ),
        Some("unavailable") => {
            println!(
                "build skew: daemon build recorded no commit (CLI {cli_sha:.12}); unverifiable"
            );
        }
        Some(sha) if cli_sha == "unavailable" => {
            println!(
                "build skew: this ck build recorded no commit (daemon {sha:.12}); unverifiable"
            );
        }
        Some(sha) if sha == cli_sha => {}
        Some(sha) => {
            println!(
                "BUILD SKEW: daemon {sha:.12} != ck {cli_sha:.12} -- one of the pair is stale;"
            );
            println!(
                "  fields this ck expects may be absent from the daemon (or arrive unrecognized)"
            );
        }
    }
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
    subc: Option<&Path>,
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
            let error = CkError::Rejected(format!(
                "unknown provider '{filter}'; valid ids: {}",
                ids.join(", ")
            ));
            if json_output {
                return Err(error);
            }
            let command = if verbose {
                "ck quota --verbose"
            } else {
                "ck quota"
            };
            return Err(CkError::WithFooter {
                error: Box::new(error),
                footer: next_step(command, "to list connected providers", subc),
            });
        }
    }

    if json_output {
        print_json(&body)?;
    } else {
        print_quota_table(&providers, provider_filter, verbose, subc);
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

/// A connected entry with money and no rate window: the balance cohort.
///
/// `usage.primary` absent + `spend` present identifies these structurally.
/// Disconnected entries (no usage object) are NOT balance-only — they keep
/// their own classification and ordering.
fn quota_entry_is_balance_only(entry: &Value) -> bool {
    let windowless = entry
        .get("usage")
        .and_then(Value::as_object)
        .is_some_and(|usage| !usage.get("primary").is_some_and(Value::is_object));
    let has_spend = entry
        .get("spend")
        .and_then(Value::as_array)
        .is_some_and(|pools| !pools.is_empty());
    windowless && has_spend
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
    // Balance-only providers (money, no rate window) sort AFTER the windowed
    // cohort so credit lines group at the end instead of landing mid-column
    // between percentages and reset timers. The cohort test is structural
    // (window absence + spend presence), never a provider-name list: a name
    // list is complete on the day it is written and silently wrong when the
    // next balance provider lands — the world changes, the list does not.
    entries.sort_by_key(|entry| (quota_entry_is_balance_only(entry), provider_id(entry)));
    entries
        .into_iter()
        .filter(|entry| {
            let matches_filter =
                filter.is_none() || filter.is_some_and(|wanted| provider_id(entry) == wanted);
            // The default view shows connected providers PLUS classified
            // degraded ones. A provider whose every account is degraded used to
            // vanish here entirely, and its absence read as UNCONFIGURED -- the
            // one meaning it definitely is not, and the reading that sends the
            // operator to re-check bindings instead of the credential (QTA,
            // from insula#8: an Anthropic lane went credential_unusable and the
            // whole Claude section disappeared). Inert entries -- never
            // configured, or a producer predating errorClass -- stay summary-
            // only so an idle fleet does not render as a wall of alarms.
            matches_filter
                && (filter.is_some()
                    || verbose
                    || quota_entry_is_connected(entry)
                    || quota_disconnect_kind(entry) != QuotaDisconnectKind::Inert)
        })
        .collect()
}

fn quota_entry_not_running_locally(entry: &Value) -> bool {
    entry.get("errorClass").and_then(Value::as_str) == Some("local_source_unavailable")
        || entry
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| {
                error.contains("no Antigravity language server")
                    || error.contains("no local provider process")
            })
}

fn print_quota_table(
    providers: &[Value],
    filter: Option<&str>,
    verbose: bool,
    subc: Option<&Path>,
) {
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

    // An empty provider array is never "nothing configured": a host with no
    // usable credentials still returns a full array of unavailable entries, so
    // the only way to reach zero is a cold module or a structural failure
    // upstream. Saying so beats printing a bare header that reads as "all
    // quiet".
    if order.is_empty() {
        let reason = quota_empty_reason(providers.is_empty(), filter.is_some());
        println!("{}", dim_text(reason, color_enabled));
        let next_step = if providers.is_empty() {
            next_step(
                "ck module status <module-id>",
                "to check the quota module",
                subc,
            )
        } else if filter.is_some() {
            next_step(
                "ck quota --verbose <provider-id>",
                "to list unavailable accounts",
                subc,
            )
        } else {
            next_step("ck quota --verbose", "to list unavailable providers", subc)
        };
        print_help_footer(&[next_step]);
        return;
    }

    let mut first_provider = true;
    for id in order {
        if !first_provider {
            println!();
        }
        first_provider = false;
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
        if group
            .iter()
            .all(|entry| quota_entry_not_running_locally(entry))
        {
            println!(
                "{} {}",
                color_text(&format_provider_display_name(&id), "1;36", color_enabled),
                dim_text("— not running locally", color_enabled)
            );
            continue;
        }
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
        // Only Inert entries are summary-only now: classified degraded entries
        // render as named sections above, so counting them here again would
        // double-report and re-bury the named line under a number.
        let inert = providers
            .iter()
            .filter(|entry| {
                !quota_entry_is_connected(entry)
                    && quota_disconnect_kind(entry) == QuotaDisconnectKind::Inert
            })
            .count();
        if inert > 0 {
            println!();
            println!(
                "{}",
                dim_text(&format!("{inert} providers not configured"), color_enabled)
            );
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

    // Money renders beside window rows, not among them: a balance has no
    // period, so it gets its own line under the account header instead of a
    // progress bar. An entry with pools and no windows is HEALTHY (deepseek's
    // balance-only shape) -- "no limits reported" stays only for entries with
    // neither pools nor windows.
    let spend_lines = quota_spend_lines_for_entry(entry);
    for line in &spend_lines {
        println!("      {line}");
    }
    let rows = quota_window_rows_for_entry(entry);
    if rows.is_empty() {
        if spend_lines.is_empty() {
            println!("      {}", dim_text("no limits reported", color_enabled));
        }
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
    if let Some(segment) = quota_stale_segment(entry) {
        extras.push(segment);
    }
    extras
}

/// Render a `stale` disclosure: this entry is a preserved last-known-good
/// reading served through an ongoing refresh failure.
///
/// The rendered duration is the BLIND time (now - stale.since: how long the
/// producer has been unable to look), deliberately not the reading's age
/// (now - fetchedAt, rendered separately above). The reading was taken, then
/// the failure began; conflating the two is the confusion the wire field
/// exists to prevent, so the renderer must not reintroduce it.
fn quota_stale_segment(entry: &Value) -> Option<String> {
    let stale = entry.get("stale")?;
    let class = stale
        .get("class")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        // The producer may preserve a reading for a reason it cannot
        // classify; the state is still worth disclosing.
        .unwrap_or("cause unstated");
    let since_raw = stale.get("since").and_then(Value::as_str);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let duration = match since_raw.and_then(parse_rfc3339_to_utc_secs) {
        // `since` is always at or after the reading was taken, so a future
        // `since` means a producer or clock defect: show the raw value
        // instead of a fabricated duration.
        Some(since) if since <= now => format_duration_two_units(now - since),
        _ => format!("since {}", since_raw.unwrap_or("unknown")),
    };
    Some(format!(
        "\u{26a0} last good reading \u{b7} refresh failing for {duration} ({class})"
    ))
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

/// Render an entry's `spend` pools as display lines, one per pool.
///
/// `spend` is money, not a rate window: a balance has no period, so pools
/// render as separate lines rather than progress bars (a bar implies a
/// window that resets; prepaid credit does not). Three wire states are
/// deliberately distinct and must stay so:
/// - `spend` ABSENT: the producer has nothing to say -> no lines.
/// - `spend: []`: the producer asked and the provider has no credit
///   product on this account -> no lines (NOT "0 credit").
/// - `spend: [...]`: one line per pool.
///
/// Pools are account-scoped; callers must never sum them across sibling
/// accounts of a provider (credit is bought per account, so a cross-account
/// total is a figure no credential can draw on). `unit` is a free string,
/// not necessarily a currency code (MiniMax reports "credit"), so it is
/// rendered verbatim after the amount.
fn quota_spend_lines_for_entry(entry: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(pools) = entry.get("spend").and_then(Value::as_array) else {
        return lines;
    };
    for pool in pools {
        let Some(remaining) = pool.get("remaining").and_then(Value::as_object) else {
            continue;
        };
        let (Some(minor), Some(exponent)) = (
            remaining.get("minor").and_then(Value::as_i64),
            remaining.get("exponent").and_then(Value::as_i64),
        ) else {
            continue;
        };
        let unit = remaining.get("unit").and_then(Value::as_str).unwrap_or("");
        let amount = format_minor_amount(minor, exponent);
        let label = pool
            .get("funding")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|funding| format!(" ({funding})"))
            .unwrap_or_default();
        lines.push(
            format!("credit {amount} {unit}{label}")
                .trim_end()
                .to_string(),
        );
    }
    lines
}

/// Render a minor-unit amount at the given decimal exponent without
/// floating point: 2402 at exponent 2 is "24.02", never "24.019999...".
/// A negative or zero exponent renders the integer as-is (no known
/// producer emits one, but a wire value must not panic the renderer).
fn format_minor_amount(minor: i64, exponent: i64) -> String {
    if exponent <= 0 {
        return minor.to_string();
    }
    let scale = 10_i64.checked_pow(exponent.min(18) as u32).unwrap_or(1);
    let sign = if minor < 0 { "-" } else { "" };
    let magnitude = minor.unsigned_abs();
    let whole = magnitude / scale as u64;
    let frac = magnitude % scale as u64;
    format!(
        "{sign}{whole}.{frac:0width$}",
        width = exponent.min(18) as usize
    )
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

fn print_module_table(modules: &[Value], verbose: bool) {
    if verbose {
        let rows = modules
            .iter()
            .map(|module| {
                vec![
                    display_field(module, "module_id"),
                    display_field(module, "state"),
                    enabled_word(module.get("enabled").and_then(Value::as_bool)),
                    running_word(module.get("live").and_then(Value::as_bool)),
                    human_health_status(&display_field(module, "health")),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["module", "state", "enabled", "live", "health"], rows);
        return;
    }

    let rows = modules
        .iter()
        .map(|module| {
            vec![
                display_field(module, "module_id"),
                module_status_text(module),
                human_health_status(&display_field(module, "health")),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["module", "status", "health"], rows);
}

fn print_rescan_table(result: &Value) {
    let preview = result
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !preview {
        print_applied_rescan_table(result);
        return;
    }

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
    };
    let added = module_ids("added");
    let removed = module_ids("removed");
    let changed = module_ids("changed_pending_reload");
    let enabled = module_ids("enabled_changes");
    let restart_required = module_ids("restart_required");
    let warnings = result
        .get("capability_warnings")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();

    if added.is_none()
        && removed.is_none()
        && changed.is_none()
        && enabled.is_none()
        && restart_required.is_none()
        && warnings.is_empty()
    {
        let unchanged = result.get("unchanged").and_then(Value::as_u64).unwrap_or(0);
        println!("no changes: {unchanged} modules match subc.jsonc");
        return;
    }

    if let Some(ids) = added {
        println!("{}: {ids}", if preview { "would add" } else { "added" });
    }
    if let Some(ids) = removed {
        println!(
            "{}: {ids}",
            if preview { "would remove" } else { "removed" }
        );
    }
    if let Some(ids) = changed {
        println!(
            "{}: {ids}",
            if preview {
                "would restart"
            } else {
                "restarted"
            }
        );
    }
    if let Some(ids) = enabled {
        println!(
            "{}: {ids}",
            if preview {
                "would change enabled state"
            } else {
                "changed enabled state"
            }
        );
    }
    if let Some(sections) = restart_required {
        println!("needs daemon restart: {sections}");
        println!("ignored sections: {sections}");
    }
    for warning in warnings {
        println!("warning: {warning}");
    }
}

fn print_applied_rescan_table(result: &Value) {
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
    if let Some(warnings) = result.get("capability_warnings").and_then(Value::as_array) {
        for warning in warnings.iter().filter_map(Value::as_str) {
            println!("warning: {warning}");
        }
    }
    let restart_required = module_ids("restart_required");
    if restart_required != "-" {
        println!("needs daemon restart: {restart_required}");
    }
}

fn print_status_table(
    module: &Value,
    health: Option<&Value>,
    frame_drops: Option<u64>,
    observed: Option<&Value>,
    verbose: bool,
) {
    let module_id = display_field(module, "module_id");
    let health_status = health
        .map(|entry| display_field(entry, "status"))
        .filter(|value| value != "-")
        .unwrap_or_else(|| display_field(module, "health"));
    println!(
        "{module_id} — {}, {}",
        module_status_text(module),
        health_sentence_status(&health_status)
    );

    let pid = observed
        .and_then(|value| value.get("pid"))
        .map(display_json_value)
        .unwrap_or_else(|| "none".to_string());
    let started = observed
        .and_then(|value| value.get("spawned_at_ms"))
        .and_then(Value::as_u64)
        .map(format_age_from_epoch_ms_now)
        .unwrap_or_else(|| "unknown".to_string());
    println!(
        "  pid {pid} · started {started} · restarts {}",
        format_restart_budget(module)
    );
    println!("  last exit: {}", format_last_exit(module));

    let binary = observed
        .and_then(|value| value.get("spawned_from"))
        .and_then(Value::as_str)
        .map(|path| display_home_path(Path::new(path)))
        .unwrap_or_else(|| "none".to_string());
    let image = observed
        .and_then(|value| value.get("running_image"))
        .map(running_image_clause)
        .unwrap_or_else(|| "running image status unknown".to_string());
    println!("  binary: {binary} ({image})");
    if health_status != "ok" {
        if let Some(detail) = health.and_then(health_operator_detail) {
            println!("  health: {detail}");
        }
    }

    if verbose {
        let failures = health
            .and_then(|entry| entry.get("consecutive_failures"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let last_action = health
            .and_then(|entry| entry.get("last_action"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(humanize_identifier)
            .unwrap_or_else(|| "none".to_string());
        let drops = frame_drops
            .map(|count| count.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        println!(
            "  supervision: {} · {} · {failures} consecutive failures · last action {last_action} · {drops} frame drops",
            enabled_word(module.get("enabled").and_then(Value::as_bool)),
            running_word(module.get("live").and_then(Value::as_bool)),
        );
        if health_status == "ok" {
            if let Some(detail) = health.and_then(health_operator_detail) {
                println!("  health: {detail}");
            }
        }
        if let Some(metrics) = health.and_then(|entry| entry.get("metrics")) {
            println!("  metrics:");
            print_metrics_tree(metrics, 2);
        }
    }
    println!("metrics: run `ck health {module_id}`");
}

fn module_status_text(module: &Value) -> String {
    if module.get("enabled").and_then(Value::as_bool) == Some(false) {
        return "stopped".to_string();
    }
    let state = module
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match state {
        "disabled" | "stopped" => "stopped".to_string(),
        "spawning" | "starting" => "starting".to_string(),
        "restarting" => "restarting".to_string(),
        "failed" => {
            let mut detail = Vec::new();
            if let Some(exit) = format_exit(
                module.get("last_exit_signal").and_then(Value::as_i64),
                module.get("last_exit_code").and_then(Value::as_i64),
            ) {
                detail.push(exit);
            }
            if let Some(restarts) = module.get("restart_count").and_then(Value::as_u64) {
                let noun = if restarts == 1 { "restart" } else { "restarts" };
                detail.push(format!("{restarts} {noun}"));
            }
            if detail.is_empty() {
                "failed".to_string()
            } else {
                format!("failed ({})", detail.join(", "))
            }
        }
        "running" if module.get("live").and_then(Value::as_bool) == Some(false) => {
            "starting".to_string()
        }
        other => humanize_identifier(other),
    }
}

fn enabled_word(value: Option<bool>) -> String {
    match value {
        Some(true) => "enabled".to_string(),
        Some(false) => "disabled".to_string(),
        None => "-".to_string(),
    }
}

fn running_word(value: Option<bool>) -> String {
    match value {
        Some(true) => "running".to_string(),
        Some(false) => "stopped".to_string(),
        None => "-".to_string(),
    }
}

fn human_health_status(status: &str) -> String {
    match status {
        "failed" | "unresponsive" => "unhealthy".to_string(),
        other => other.to_string(),
    }
}

fn health_sentence_status(status: &str) -> String {
    match status {
        "ok" => "healthy".to_string(),
        "failed" | "unresponsive" => "unhealthy".to_string(),
        other => humanize_identifier(other),
    }
}

fn running_image_clause(image: &Value) -> String {
    match image.get("status").and_then(Value::as_str) {
        Some("match") => "running image matches".to_string(),
        Some("mismatch") => "running image differs: running vs disk".to_string(),
        Some("unavailable") => format!(
            "running image differs: {}",
            provenance_value(image.get("reason"))
        ),
        Some(other) => format!("running image status {}", humanize_identifier(other)),
        None => "running image status unknown".to_string(),
    }
}

fn format_exit(signal: Option<i64>, code: Option<i64>) -> Option<String> {
    match (signal, code) {
        (Some(signal), _) => Some(format!("signal {signal}")),
        (None, Some(code)) => Some(format!("exit {code}")),
        (None, None) => None,
    }
}

fn humanize_identifier(value: &str) -> String {
    value.replace(['_', '-'], " ")
}

fn module_frame_drop_count(describe: &Value, module_id: &str) -> Option<u64> {
    describe
        .get("counters")
        .and_then(Value::as_object)?
        .get("module_frames_dropped_no_route_by_module")
        .and_then(Value::as_object)?
        .get(module_id)
        .and_then(Value::as_u64)
        .filter(|count| *count > 0)
}

fn format_restart_budget(module: &Value) -> String {
    match (
        module.get("restart_count").and_then(Value::as_u64),
        module.get("max_restarts").and_then(Value::as_u64),
        module.get("lifetime_restarts").and_then(Value::as_u64),
    ) {
        (Some(used), Some(allowed), Some(lifetime)) if lifetime != used => {
            format!("{used} of {allowed} ({lifetime} lifetime)")
        }
        (Some(used), Some(allowed), _) => format!("{used} of {allowed}"),
        _ => "unknown".to_string(),
    }
}

fn format_last_exit(module: &Value) -> String {
    let exit = format_exit(
        module.get("last_exit_signal").and_then(Value::as_i64),
        module.get("last_exit_code").and_then(Value::as_i64),
    );
    let Some(exit) = exit else {
        return "none".to_string();
    };
    match module.get("last_exit_ms").and_then(Value::as_u64) {
        Some(at_ms) => format!("{exit}, {}", format_age_from_epoch_ms_now(at_ms)),
        None => exit,
    }
}

fn print_health_table(modules: &[Value], verbose: bool, subc: Option<&Path>) {
    if modules.is_empty() {
        println!("no supervised modules");
        let footer = [next_step(
            "ck module rescan",
            "to reconcile configured modules",
            subc,
        )];
        print_help_footer(&footer);
        return;
    }

    let id_width = modules
        .iter()
        .map(|module| display_field(module, "module_id").chars().count())
        .max()
        .unwrap_or(0);
    let status_width = modules
        .iter()
        .map(|module| {
            human_health_status(&display_field(module, "status"))
                .chars()
                .count()
        })
        .max()
        .unwrap_or(0);

    for module in modules {
        let id = display_field(module, "module_id");
        let status = human_health_status(&display_field(module, "status"));
        let mut details = health_operator_detail(module)
            .into_iter()
            .collect::<Vec<_>>();
        if verbose {
            let failures = module
                .get("consecutive_failures")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            details.push(format!("{failures} consecutive failures"));
            if let Some(action) = module
                .get("last_action")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                details.push(format!("last action {}", humanize_identifier(action)));
            }
            if let Some(age_s) = health_record_age_secs(module) {
                details.push(format!(
                    "record {} old",
                    format_duration(Duration::from_secs(age_s))
                ));
            }
        }
        let detail = if details.is_empty() {
            "-".to_string()
        } else {
            details.join(" · ")
        };
        println!("{id:<id_width$}  {status:<status_width$}  {detail}");
    }
}

fn health_operator_detail(entry: &Value) -> Option<String> {
    let metrics = entry.get("metrics").and_then(Value::as_object);
    let degraded = metrics
        .and_then(|metrics| metrics.get("degraded"))
        .and_then(Value::as_array);
    let unconfigured = metrics
        .and_then(|metrics| metrics.get("unconfigured"))
        .and_then(Value::as_array);
    if degraded.is_some() || unconfigured.is_some() {
        let degraded = degraded
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        let unconfigured_count = unconfigured.map(Vec::len).unwrap_or(0);
        let provider = if degraded.len() == 1 {
            "provider"
        } else {
            "providers"
        };
        let degraded_detail = if degraded.is_empty() {
            "no providers degraded".to_string()
        } else {
            format!(
                "{} {provider} degraded ({})",
                degraded.len(),
                degraded.join(", ")
            )
        };
        return Some(format!(
            "{degraded_detail}; {unconfigured_count} not configured"
        ));
    }

    // A detail that only restates the status (`ok` beside `ok`) tells the
    // reader nothing; the row reads cleaner with the absence mark.
    let status = entry.get("status").and_then(Value::as_str);
    entry
        .get("detail")
        .and_then(Value::as_str)
        .filter(|detail| !detail.is_empty())
        .filter(|detail| Some(detail.trim()) != status)
        .map(str::to_string)
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

fn next_step(command: &str, explanation: &str, subc: Option<&Path>) -> String {
    let connection_flag = subc.map_or("", |_| " --subc <connection-file>");
    format!("Run `{command}{connection_flag}` {explanation}")
}

fn print_help_footer<S: AsRef<str>>(lines: &[S]) {
    let count = lines.len().min(2);
    if count == 0 {
        return;
    }
    println!("\nnext:");
    for line in lines.iter().take(count) {
        println!("  {}", line.as_ref());
    }
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

fn format_age_from_epoch_ms_now(timestamp_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(timestamp_ms);
    format_age_from_epoch_ms(timestamp_ms, now_ms)
}

fn format_age_from_epoch_ms(timestamp_ms: u64, now_ms: u64) -> String {
    if timestamp_ms > now_ms {
        let seconds = (timestamp_ms - now_ms) / 1_000;
        if seconds == 0 {
            "just now".to_string()
        } else {
            format!("in {}", format_duration(Duration::from_secs(seconds)))
        }
    } else {
        format_age_seconds((now_ms - timestamp_ms) / 1_000)
    }
}

fn format_age_seconds(seconds: u64) -> String {
    if seconds == 0 {
        "just now".to_string()
    } else {
        format!("{} ago", format_duration(Duration::from_secs(seconds)))
    }
}

/// `supervisor.rescan { preview: true }` as `ck module rescan --dry-run` uses
/// it, through this binary's existing client. Setup must not shell out to `ck`:
/// that would recurse, and an older daemon that ignores `preview` would apply a
/// real rescan. Absence of `preview: true` in the reply is therefore a failure.
pub(crate) fn preview_rescan_restart_required() -> Result<Vec<String>, String> {
    std::thread::Builder::new()
        .name("ck-setup-rescan-preview".into())
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("could not start runtime for rescan preview: {error}"))?
                .block_on(preview_rescan_restart_required_async())
        })
        .map_err(|error| format!("could not spawn rescan preview: {error}"))?
        .join()
        .map_err(|_| "rescan preview thread panicked".to_string())?
}

async fn preview_rescan_restart_required_async() -> Result<Vec<String>, String> {
    let resolved = discover_connection_file(None).map_err(|error| error.to_string())?;
    let mut client = CkClient::connect(resolved)
        .await
        .map_err(|error| error.to_string())?;
    let result = client
        .rpc_value(ClientControlRequest::SupervisorRescan { preview: true })
        .await
        .map_err(|error| error.to_string())?;
    let honoured = result
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !honoured {
        return Err(
            "this daemon does not support `rescan --dry-run` and IGNORED the flag: it may \
             have applied a real reconciliation just now. Compare `ck module list` against \
             your config, and upgrade the daemon before relying on setup to preview a restart."
                .to_string(),
        );
    }
    Ok(result
        .get("restart_required")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

fn setup_command(program: &Path, request: &setup::SetupRequest) -> Result<(), CkError> {
    let mut backend =
        setup::SetupBackend::current(program.to_path_buf()).map_err(CkError::Message)?;
    let observed = backend.observe(request).map_err(CkError::Message)?;
    let plan = setup::plan_setup(&observed, request);

    if let Some(message) = setup_unavailable_message(&plan, &observed) {
        println!("{message}");
        print_verbose_setup_outcomes(&plan, request.verbose);
        return if plan.is_authorized() {
            Ok(())
        } else {
            Err(CkError::RenderedExit { exit_code: 1 })
        };
    }
    if !plan.is_authorized() {
        let message = plan
            .outcomes
            .iter()
            .find(|outcome| outcome.blocks_execution())
            .map(human_blocking_outcome)
            .unwrap_or_else(|| "setup could not continue; nothing was installed".to_string());
        println!("{message}");
        print_verbose_setup_outcomes(&plan, request.verbose);
        return Err(CkError::RenderedExit { exit_code: 1 });
    }
    if plan.mutation_count() == 0 {
        for line in setup_no_change_summary(&observed, request) {
            println!("{line}");
        }
        print_verbose_setup_outcomes(&plan, request.verbose);
        return Ok(());
    }
    if request.dry_run {
        backend.print_dry_run(&plan).map_err(CkError::Message)?;
        backend
            .print_proposed_diffs(&plan, request)
            .map_err(CkError::Message)?;
        print_verbose_setup_outcomes(&plan, request.verbose);
        return Ok(());
    }
    backend
        .apply_plan(&plan, request)
        .map_err(CkError::Message)?;
    print_verbose_outcomes(&plan.outcomes, request.verbose);
    Ok(())
}

fn setup_no_change_summary(
    observed: &setup::SetupObserved,
    request: &setup::SetupRequest,
) -> Vec<String> {
    if request.uninstall {
        return vec!["Nothing to uninstall; no managed setup files are present.".to_string()];
    }

    let mut installed = setup::Component::ALL
        .into_iter()
        .filter(|component| *component != setup::Component::Core)
        .filter(|component| observed.component_state(*component) == setup::ComponentState::Correct)
        .map(|component| component.module_id().unwrap_or(component.label()))
        .collect::<Vec<_>>();
    installed.sort_unstable();
    let mut lines = vec![if installed.is_empty() {
        "CortexKit is set up: daemon running.".to_string()
    } else {
        format!(
            "CortexKit is set up: daemon running, {} ok.",
            installed.join(" · ")
        )
    }];

    let mut missing = setup::Component::ALL
        .into_iter()
        .filter(|component| *component != setup::Component::Core)
        .filter(|component| observed.component_state(*component) != setup::ComponentState::Correct)
        .map(setup::Component::label)
        .collect::<Vec<_>>();
    missing.sort_unstable();
    if !missing.is_empty() {
        let commands = missing
            .iter()
            .map(|component| format!("`ck setup {component}`"))
            .collect::<Vec<_>>()
            .join(" or ");
        lines.push(format!(
            "Optional modules not installed: {} — run {commands}.",
            missing.join(", ")
        ));
    }
    lines
}

fn setup_unavailable_message(
    plan: &setup::SetupPlan,
    observed: &setup::SetupObserved,
) -> Option<String> {
    plan.outcomes.iter().find_map(|outcome| match outcome {
        setup::PlanOutcome::ReleaseIncomplete { component, .. } => {
            let target = match &observed.platform {
                setup::PlatformObservation::Supported(target) => target.to_string(),
                setup::PlatformObservation::Unsupported(target) => target.to_string(),
            };
            Some(format!(
                "{} has no {target} release yet; nothing was installed.",
                component.label()
            ))
        }
        setup::PlanOutcome::DeclaredUnavailable { component, .. } => Some(format!(
            "{} is not available on this platform yet; nothing was installed.",
            component.module_id().unwrap_or(component.label())
        )),
        _ => None,
    })
}

fn human_blocking_outcome(outcome: &setup::PlanOutcome) -> String {
    match outcome {
        setup::PlanOutcome::Refusal { reason } => reason.clone(),
        _ => outcome.to_string(),
    }
}

fn print_verbose_setup_outcomes(plan: &setup::SetupPlan, verbose: bool) {
    if verbose {
        for line in plan
            .render()
            .lines()
            .filter(|line| line.trim_start().starts_with("outcome:"))
        {
            println!("{line}");
        }
    }
}

fn print_verbose_outcomes(outcomes: &[setup::PlanOutcome], verbose: bool) {
    if verbose {
        for outcome in outcomes {
            println!("  outcome: {outcome}");
        }
    }
}

async fn upgrade_command(
    executable: &Path,
    subc: Option<&Path>,
    check: bool,
    verbose: bool,
) -> Result<(), CkError> {
    // The daemon connection file is the daemon catalog build evidence. It is
    // optional while no inventory-owned daemon exists; discovery turns its
    // absence into a refusal only when an owned daemon actually needs a version.
    let daemon_catalog =
        discover_connection_file(subc)
            .ok()
            .map(|connection| setup::DaemonCatalogBuild {
                pid: connection.info.pid,
                version: connection.info.daemon_ver,
            });
    let discovered = setup::discover_current_upgrade_targets(executable, daemon_catalog.as_ref())
        .map_err(CkError::Rejected)?;
    let cache = setup::UpdateCache::from_environment();
    let source = setup::IndexReleaseSource::from_environment(setup::TARGET_CHECK_BUDGET);
    let metadata = match setup::check_update_metadata(&cache, &source).await {
        Ok(metadata) => metadata,
        Err(error @ setup::UpdateCheckError::IndexStale { .. }) => {
            println!("{error}");
            return Err(CkError::UpdateCheck(error));
        }
        Err(error @ setup::UpdateCheckError::IndexUnreachable { .. }) => {
            println!("{}", setup::not_checked_from_cache(&cache).render());
            return Err(CkError::UpdateCheck(error));
        }
        Err(error) => return Err(CkError::UpdateCheck(error)),
    };
    let observed = setup::observed_upgrade_targets(&metadata, &discovered);
    let plan = setup::plan_upgrade(&observed);
    if !plan.is_authorized() {
        let message = plan
            .outcomes
            .iter()
            .find(|outcome| outcome.blocks_execution())
            .map(human_blocking_outcome)
            .unwrap_or_else(|| "upgrade could not continue; nothing was changed".to_string());
        println!("{message}");
        print_verbose_outcomes(&plan.outcomes, verbose);
        return Err(CkError::RenderedExit { exit_code: 1 });
    }
    let planned_mutations = plan
        .operations
        .iter()
        .filter(|operation| operation.mutates())
        .count();
    if check {
        let updates = plan
            .outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                setup::PlanOutcome::UpgradeAvailable {
                    target, from, to, ..
                } => Some(format!("{target} {from} → {to}. Run ck upgrade.")),
                _ => None,
            })
            .collect::<Vec<_>>();
        if updates.is_empty() {
            println!("{}", upgrade_current_summary(&discovered));
        } else {
            for update in updates {
                println!("{update}");
            }
        }
        print_verbose_outcomes(&plan.outcomes, verbose);
        return Ok(());
    }
    if planned_mutations == 0 {
        println!("{}", upgrade_current_summary(&discovered));
        print_verbose_outcomes(&plan.outcomes, verbose);
        return Ok(());
    }

    #[cfg(feature = "test-support")]
    if env::var_os("CK_TEST_UPGRADE_APPLY_OK").is_some() {
        for outcome in &plan.outcomes {
            if let setup::PlanOutcome::UpgradeAvailable {
                target, from, to, ..
            } = outcome
            {
                println!("{}", setup::upgraded_line(*target, from, to));
            }
        }
        println!("Done.");
        print_verbose_outcomes(&plan.outcomes, verbose);
        return Ok(());
    }

    let index = source
        .cloned_index()
        .map_err(|error| CkError::Message(error.to_string()))?;
    let mut backend = setup::SystemUpgradeBackend::new(
        executable,
        subc.map(Path::to_path_buf),
        discovered,
        index,
    )
    .map_err(CkError::Rejected)?;
    for target in setup::UpgradeTarget::ORDERED {
        if let setup::UpgradeState::UpdateAvailable { to, .. } = observed.target_state(target) {
            backend.set_expected_version(target, to);
        }
    }
    match setup::execute_upgrade(&plan, &mut backend) {
        Ok(report) => {
            println!("Done.");
            print_verbose_outcomes(&plan.outcomes, verbose);
            if verbose {
                setup::render_execution_report(&report);
            }
            Ok(())
        }
        Err(failure) => {
            setup::render_execution_report(&failure.report);
            Err(CkError::Rejected(failure.to_string()))
        }
    }
}

fn upgrade_current_summary(discovered: &[setup::ManagedUpgradeTarget]) -> String {
    let mut components = Vec::new();
    for target in [
        setup::UpgradeTarget::Ck,
        setup::UpgradeTarget::Daemon,
        setup::UpgradeTarget::SubcMcp,
        setup::UpgradeTarget::Aft,
    ] {
        let Some(item) = discovered.iter().find(|item| item.target == target) else {
            continue;
        };
        if target == setup::UpgradeTarget::SubcMcp {
            components.push(target.to_string());
        } else {
            components.push(format!("{target} {}", item.installed_version));
        }
    }
    if components.is_empty() {
        "Everything is up to date.".to_string()
    } else {
        format!("Everything is up to date ({}).", components.join(" · "))
    }
}

fn parse_setup_command(tail: &[OsString]) -> Result<setup::SetupRequest, CkError> {
    let mut optional = BTreeSet::new();
    let mut explicit_component = None;
    let mut used_with = false;
    let mut uninstall = false;
    let mut dry_run = false;
    let mut verbose = false;
    let mut convert = false;
    let mut conversion_confirmed = false;
    let mut claustrum_key_path = None;
    let mut index = 0;

    while let Some(argument) = tail.get(index) {
        let argument = argument.to_string_lossy();
        match argument.as_ref() {
            "--with" => {
                let Some(value) = tail.get(index + 1) else {
                    return Err(CkError::Usage(format!(
                        "ck setup --with requires a comma-separated component list\n\n{SETUP_HELP}"
                    )));
                };
                if used_with || explicit_component.is_some() {
                    return Err(CkError::Usage(format!(
                        "ck setup accepts either one explicit component or one --with list\n\n{SETUP_HELP}"
                    )));
                }
                used_with = true;
                let value = value.to_string_lossy();
                if value.is_empty() {
                    return Err(CkError::Usage(format!(
                        "ck setup --with requires at least one component\n\n{SETUP_HELP}"
                    )));
                }
                for component in value.split(',') {
                    let component = parse_setup_component(component)?;
                    if !optional.insert(component) {
                        return Err(CkError::Usage(format!(
                            "ck setup --with repeats component '{component}'\n\n{SETUP_HELP}"
                        )));
                    }
                }
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--verbose" => {
                verbose = true;
                index += 1;
            }
            "--uninstall" => {
                uninstall = true;
                index += 1;
            }
            "--convert" => {
                convert = true;
                index += 1;
            }
            "--confirm" => {
                conversion_confirmed = true;
                index += 1;
            }
            "--key-path" => {
                let Some(value) = tail.get(index + 1) else {
                    return Err(CkError::Usage(format!(
                        "ck setup --key-path requires a file path\n\n{SETUP_HELP}"
                    )));
                };
                if claustrum_key_path.is_some() {
                    return Err(CkError::Usage(format!(
                        "ck setup accepts one --key-path\n\n{SETUP_HELP}"
                    )));
                }
                claustrum_key_path = Some(PathBuf::from(value));
                index += 2;
            }
            value if value.starts_with('-') => {
                return Err(CkError::Usage(format!(
                    "unknown setup flag '{value}'\n\n{SETUP_HELP}"
                )));
            }
            value => {
                if used_with || explicit_component.is_some() {
                    return Err(CkError::Usage(format!(
                        "ck setup accepts one optional component\n\n{SETUP_HELP}"
                    )));
                }
                let component = parse_setup_component(value)?;
                optional.insert(component);
                explicit_component = Some(component);
                index += 1;
            }
        }
    }

    if uninstall && (!optional.is_empty() || convert || conversion_confirmed) {
        return Err(CkError::Usage(format!(
            "ck setup --uninstall cannot be combined with component installation or conversion\n\n{SETUP_HELP}"
        )));
    }
    if convert && explicit_component.is_none() {
        return Err(CkError::Usage(format!(
            "ck setup --convert requires 'aft' or 'mc'\n\n{SETUP_HELP}"
        )));
    }
    if conversion_confirmed && !convert {
        return Err(CkError::Usage(format!(
            "ck setup --confirm is only valid with --convert\n\n{SETUP_HELP}"
        )));
    }
    if claustrum_key_path.is_some()
        && explicit_component != Some(setup::Component::Claustrum)
        && !optional.contains(&setup::Component::Claustrum)
    {
        return Err(CkError::Usage(format!(
            "ck setup --key-path is only valid for claustrum\n\n{SETUP_HELP}"
        )));
    }

    let claustrum_selected = explicit_component == Some(setup::Component::Claustrum)
        || optional.contains(&setup::Component::Claustrum);
    let mut request = setup::SetupRequest::install(optional.into_iter().collect());
    request.uninstall = uninstall;
    request.dry_run = dry_run;
    request.verbose = verbose;
    request.convert = if convert { explicit_component } else { None };
    request.conversion_confirmed = conversion_confirmed;
    if claustrum_selected {
        request.claustrum_key_path = match claustrum_key_path {
            Some(path) => Some(path),
            None => setup::default_claustrum_key_path().map_err(CkError::Message)?,
        };
    }
    Ok(request)
}

fn parse_setup_component(value: &str) -> Result<setup::Component, CkError> {
    match value {
        "aft" => Ok(setup::Component::Aft),
        "mc" => Ok(setup::Component::Mc),
        "insula" => Ok(setup::Component::Insula),
        "claustrum" => Ok(setup::Component::Claustrum),
        "synapse" => Ok(setup::Component::Synapse),
        _ => Err(CkError::Usage(format!(
            "unknown setup component '{value}'; expected aft, mc, insula, claustrum, or synapse\n\n{SETUP_HELP}"
        ))),
    }
}

fn parse_upgrade_command(tail: &[OsString]) -> Result<Command, CkError> {
    let mut check = false;
    let mut verbose = false;
    for argument in tail {
        match argument.to_string_lossy().as_ref() {
            "--check" if !check => check = true,
            "--verbose" if !verbose => verbose = true,
            value => {
                return Err(CkError::Usage(format!(
                    "unknown or repeated upgrade flag '{value}'\n\n{UPGRADE_HELP}"
                )))
            }
        }
    }
    Ok(Command::Upgrade { check, verbose })
}

fn parse_args(argv: impl IntoIterator<Item = OsString>) -> Result<CkArgs, CkError> {
    let mut args = argv.into_iter();
    let program = args
        .next()
        .map(PathBuf::from)
        .or_else(|| env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("ck"));
    let mut subc = None;
    let mut json = false;
    let mut verbose = false;

    // Dispatcher-local flags are only parsed BEFORE the domain; everything from
    // the first positional on is the command tail (an unknown domain forwards it
    // verbatim to the external ck-<domain> binary, flags and all).
    let domain: String = loop {
        match args.next() {
            None => {
                return Ok(CkArgs {
                    program,
                    subc,
                    json,
                    verbose,
                    command: if json {
                        Command::Help(top_help())
                    } else {
                        Command::Dashboard
                    },
                })
            }
            Some(arg) if arg == OsStr::new("--subc") => {
                subc = Some(PathBuf::from(take_value(&mut args, "--subc")?));
            }
            Some(arg) if arg == OsStr::new("--json") => json = true,
            Some(arg) if arg == OsStr::new("--verbose") => verbose = true,
            Some(arg) if arg == OsStr::new("-h") || arg == OsStr::new("--help") => {
                return Ok(CkArgs {
                    program: program.clone(),
                    subc,
                    json,
                    verbose,
                    command: Command::Help(top_help()),
                })
            }
            Some(arg) if arg == OsStr::new("--version") || arg == OsStr::new("-V") => {
                return Ok(CkArgs {
                    program: program.clone(),
                    subc,
                    json,
                    verbose,
                    command: Command::Help(format!("ck {}", env!("CARGO_PKG_VERSION"))),
                })
            }
            Some(arg) if arg == OsStr::new("--ck-build-shape") => {
                return Ok(CkArgs {
                    program: program.clone(),
                    subc,
                    json,
                    verbose,
                    command: Command::BuildShape,
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
    verbose |= raw_tail.iter().any(|arg| arg == "--verbose");

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
        program,
        subc,
        json,
        verbose,
        command,
    })
}

fn is_builtin_domain(domain: &str) -> bool {
    matches!(
        domain,
        "setup"
            | "upgrade"
            | "module"
            | "routes"
            | "provenance"
            | "health"
            | "daemon"
            | "quota"
            | "help"
    )
}

fn parse_command(domain: &str, tail: &[OsString]) -> Result<Command, CkError> {
    // Built-in domains parse their verbs strictly and answer verbless/misused
    // invocations with the DOMAIN's help, not the whole command tree.
    match domain {
        "help" => {
            let topic = tail.first().map(|t| t.to_string_lossy());
            Ok(Command::Help(match topic.as_deref() {
                Some("setup") => SETUP_HELP.into(),
                Some("upgrade") => UPGRADE_HELP.into(),
                Some("module") => MODULE_HELP.into(),
                Some("routes") => ROUTES_HELP.into(),
                Some("provenance") => PROVENANCE_HELP.into(),
                Some("quota") => QUOTA_HELP.into(),
                Some("health") => HEALTH_HELP.into(),
                Some("daemon") => DAEMON_HELP.into(),
                _ => top_help(),
            }))
        }
        "setup"
            if tail
                .iter()
                .any(|arg| arg == "-h" || arg == "--help" || arg == "help") =>
        {
            Ok(Command::Help(SETUP_HELP.into()))
        }
        "setup" => parse_setup_command(tail).map(Command::Setup),
        "upgrade"
            if tail
                .iter()
                .any(|arg| arg == "-h" || arg == "--help" || arg == "help") =>
        {
            Ok(Command::Help(UPGRADE_HELP.into()))
        }
        "upgrade" => parse_upgrade_command(tail),
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
                "release" => ModuleCommand::ReleaseReserved { module_id: id(1)? },
                "status" => ModuleCommand::Status { module_id: id(1)? },
                // `-n <count>` narrows the tail daemon-side rather than here, so
                // a caller asking for 20 lines is not shipped the whole ring to
                // discard most of it.
                "stderr" => ModuleCommand::StderrTail {
                    module_id: id(1)?,
                    max_lines: parse_tail_count(tail)?,
                },
                "terminals" => ModuleCommand::Terminals { module_id: id(1)? },
                // --now = don't wait for in-flight requests (a wedged request
                // never settles, so waiting only delays recovery); --drain-ms N
                // widens/narrows the wait for this one restart. Flag-less form
                // sends no override so older daemons keep accepting the request.
                "restart" => ModuleCommand::Restart {
                    module_id: id(1)?,
                    drain_timeout_ms: parse_drain_override(tail)?,
                },
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
        "routes" => {
            let positional = tail
                .iter()
                .filter(|argument| argument.as_os_str() != "--verbose")
                .collect::<Vec<_>>();
            match positional.as_slice() {
                [] => Ok(Command::Routes { module_id: None }),
                [module_id]
                    if module_id.as_os_str() != "-h"
                        && module_id.as_os_str() != "--help"
                        && module_id.as_os_str() != "help" =>
                {
                    Ok(Command::Routes {
                        module_id: Some(module_id.to_string_lossy().into_owned()),
                    })
                }
                _ => Ok(Command::Help(ROUTES_HELP.into())),
            }
        }
        "provenance" => {
            let positional = tail
                .iter()
                .filter(|argument| argument.as_os_str() != "--verbose")
                .collect::<Vec<_>>();
            let Some(module_id) = positional.first() else {
                return Ok(Command::Help(PROVENANCE_HELP.into()));
            };
            if positional.len() != 1
                || module_id.as_os_str() == "-h"
                || module_id.as_os_str() == "--help"
                || module_id.as_os_str() == "help"
            {
                return Ok(Command::Help(PROVENANCE_HELP.into()));
            }
            Ok(Command::Provenance {
                module_id: module_id.to_string_lossy().into_owned(),
            })
        }
        "health" => match tail
            .iter()
            .find(|argument| argument.as_os_str() != "--verbose")
        {
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
        "daemon" => {
            if tail
                .iter()
                .any(|arg| arg == "-h" || arg == "--help" || arg == "help")
            {
                return Ok(Command::Help(DAEMON_HELP.into()));
            }
            if tail.first().is_some_and(|verb| verb == "lint") {
                return parse_daemon_lint(&tail[1..]);
            }
            let positional = tail
                .iter()
                .filter(|argument| argument.as_os_str() != "--verbose")
                .collect::<Vec<_>>();
            match positional.as_slice() {
                [] => Ok(Command::Daemon),
                [verb] if verb.as_os_str() == "triage" => Ok(Command::DaemonTriage),
                _ => Ok(Command::Help(DAEMON_HELP.into())),
            }
        }
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

fn parse_daemon_lint(tail: &[OsString]) -> Result<Command, CkError> {
    let mut config = None;
    let mut verbose = false;
    for argument in tail {
        let argument = argument.to_string_lossy();
        if argument == "--verbose" {
            verbose = true;
        } else if argument.starts_with('-') {
            return Err(CkError::Usage(format!(
                "unknown daemon lint flag '{argument}'\n\n{DAEMON_HELP}"
            )));
        } else if config.is_none() {
            config = Some(PathBuf::from(argument.into_owned()));
        } else {
            return Err(CkError::Usage(format!(
                "ck daemon lint accepts at most one config path\n\n{DAEMON_HELP}"
            )));
        }
    }
    Ok(Command::FleetLint { config, verbose })
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

fn decorate_error(error: CkError, json_output: bool, subc: Option<&Path>) -> CkError {
    if json_output {
        return error;
    }
    let footer = match &error {
        CkError::Discovery { .. } | CkError::Connection { .. } => Some(
            "Check the connection file path above, then run `ck daemon --subc <connection-file>`"
                .to_string(),
        ),
        CkError::Rejected(message)
            if message.contains("module_id '") && message.contains("not supervised") =>
        {
            Some(next_step("ck module list", "to see valid module ids", subc))
        }
        CkError::Rejected(message) if message.contains("unknown provider '") => {
            Some(next_step("ck quota", "to list connected providers", subc))
        }
        _ => None,
    };
    match footer {
        Some(footer) => CkError::WithFooter {
            error: Box::new(error),
            footer,
        },
        None => error,
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
    Discovery {
        tried: Vec<TriedConnectionFile>,
    },
    Connection {
        path: PathBuf,
        source: String,
    },
    Rejected(String),
    ModuleNotCommand {
        command: String,
        module_id: String,
    },
    WithFooter {
        error: Box<CkError>,
        footer: String,
    },
    Message(String),
    FleetLintConfig(String),
    UpdateCheck(setup::UpdateCheckError),
    /// The report was written to stdout; exit silently with lint's classification.
    FleetLintExit {
        exit_code: i32,
    },
    /// Triage prints its complete report before returning its classification.
    TriageExit {
        exit_code: i32,
    },
    /// The command already printed its complete human-facing refusal.
    RenderedExit {
        exit_code: i32,
    },
    Json(serde_json::Error),
}

impl CkError {
    fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) | Self::Discovery { .. } => 2,
            Self::Connection { .. } => 3,
            Self::Rejected(_) | Self::Message(_) | Self::Json(_) | Self::UpdateCheck(_) => 1,
            Self::ModuleNotCommand { .. } => 1,
            Self::FleetLintConfig(_) => 2,
            Self::FleetLintExit { exit_code }
            | Self::TriageExit { exit_code }
            | Self::RenderedExit { exit_code } => *exit_code,
            Self::WithFooter { error, .. } => error.exit_code(),
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
            Self::ModuleNotCommand { command, module_id } => write!(
                f,
                "'{command}' is a module, not a command. Try: ck module status {module_id}"
            ),
            Self::Message(message) => write!(f, "{message}"),
            Self::FleetLintConfig(message) => write!(f, "ck daemon lint: {message}"),
            Self::UpdateCheck(error) => error.fmt(f),
            Self::FleetLintExit { .. } | Self::TriageExit { .. } | Self::RenderedExit { .. } => {
                Ok(())
            }
            Self::Json(source) => write!(f, "json: {source}"),
            Self::WithFooter { error, footer } => {
                write!(f, "{error}\n\nnext:\n  {footer}")
            }
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
    /// A module whose detail string is just its status again (synapse sends
    /// `detail: "ok"`) rendered `ok  ok` on the health overview; a real detail
    /// still shows, and an absent one stays absent.
    #[test]
    fn health_detail_that_restates_the_status_is_elided() {
        let restated = json!({"status": "ok", "detail": "ok"});
        assert_eq!(health_operator_detail(&restated), None);
        let real = json!({"status": "ok", "detail": "3 roots warming"});
        assert_eq!(
            health_operator_detail(&real).as_deref(),
            Some("3 roots warming")
        );
        let absent = json!({"status": "ok"});
        assert_eq!(health_operator_detail(&absent), None);
    }

    /// Balance-only providers (spend, no primary window) sort AFTER windowed
    /// ones; within each cohort the order stays alphabetical. The cohort test
    /// is structural, so a NEW balance provider lands at the end without a
    /// name list knowing about it.
    #[test]
    fn module_binary_names_include_every_setup_module_program() {
        let modules = module_command_ids();
        assert_eq!(
            modules.get("mc").map(String::as_str),
            Some("magic-context"),
            "MC must point at its daemon module id"
        );
        assert_eq!(
            modules.get("synapse").map(String::as_str),
            Some("synapse"),
            "synapse must remain a module command rejection"
        );
    }

    #[test]
    fn setup_and_upgrade_identity_does_not_use_a_bare_argv_zero() {
        let argv_zero = Path::new("ck");
        let identity = running_executable().expect("test binary has an executable identity");

        assert!(identity.is_absolute());
        assert_ne!(identity, argv_zero);
    }

    #[test]
    fn balance_only_providers_sort_to_the_end() {
        let providers = vec![
            serde_json::json!({"provider": "deepseek", "usage": {}, "spend": [{"amount": 2402, "unit": "CNY"}]}),
            serde_json::json!({"provider": "claude", "usage": {"primary": {"usedPercent": 40}}}),
            serde_json::json!({"provider": "zenmux-new", "usage": {}, "spend": [{"amount": 5, "unit": "USD"}]}),
            serde_json::json!({"provider": "gemini", "usage": {"primary": {"usedPercent": 10}}}),
        ];
        let order: Vec<String> = quota_entries_for_table(&providers, None, false)
            .iter()
            .map(|entry| provider_id(entry))
            .collect();
        assert_eq!(order, ["claude", "gemini", "deepseek", "zenmux-new"]);
        // Windowed entries are NOT balance-only even when a spend pool rides along:
        let mixed = serde_json::json!({"provider": "x", "usage": {"primary": {"usedPercent": 1}}, "spend": [{"amount": 1}]});
        assert!(!quota_entry_is_balance_only(&mixed));
    }

    mod drain_override {
        use super::super::parse_drain_override;
        use std::ffi::OsString;

        fn tail(tokens: &[&str]) -> Vec<OsString> {
            tokens.iter().map(OsString::from).collect()
        }

        #[test]
        fn absent_flags_send_no_override() {
            // None keeps the wire request byte-identical to pre-field clients,
            // which is what lets older daemons keep accepting it.
            assert_eq!(parse_drain_override(&tail(&["aft"])).unwrap(), None);
        }

        #[test]
        fn now_is_zero_and_drain_ms_parses() {
            assert_eq!(
                parse_drain_override(&tail(&["aft", "--now"])).unwrap(),
                Some(0)
            );
            assert_eq!(
                parse_drain_override(&tail(&["aft", "--drain-ms", "120000"])).unwrap(),
                Some(120_000)
            );
        }

        #[test]
        fn conflicting_flags_and_bad_counts_are_refused() {
            assert!(parse_drain_override(&tail(&["aft", "--now", "--drain-ms", "5"])).is_err());
            assert!(parse_drain_override(&tail(&["aft", "--drain-ms"])).is_err());
            assert!(parse_drain_override(&tail(&["aft", "--drain-ms", "soon"])).is_err());
        }
    }

    use super::*;
    use subc_control::{StderrCaptureState, StderrTail, StderrTailEntry};
    use subc_core::test_support::TestTempDir;

    #[test]
    fn provenance_value_escapes_terminal_controls() {
        for value in [
            "\u{1b}]52;c;AAAA\u{07}",
            "\u{1b}[2J",
            "\u{07}wire",
            "schema\u{0a}",
        ] {
            let value = Value::String(value.to_string());
            let escaped = provenance_value(Some(&value));
            assert!(!escaped.bytes().any(|byte| byte < 0x20));
            assert!(
                escaped.contains(r"\x1b") || escaped.contains(r"\x07") || escaped.contains(r"\x0a")
            );
        }
    }

    #[test]
    fn provenance_image_renders_unknown_reason_escaped() {
        let value = serde_json::json!({
            "status": "unavailable",
            "reason": "future_reason\u{1b}]52;c;AAAA\u{07}"
        });

        let rendered = provenance_image(Some(&value));

        assert!(rendered
            .starts_with("could not be compared with the file it was spawned from (future_reason"));
        assert!(rendered.contains(r"\x1b"));
        assert!(rendered.contains(r"\x07"));
        assert!(!rendered.bytes().any(|byte| byte < 0x20));
    }

    #[test]
    fn provenance_image_renders_unknown_tag_and_body_escaped() {
        let value = serde_json::json!({
            "status": "future_status\u{1b}]52;c;AAAA\u{07}",
            "detail": "future detail\u{1b}[2J"
        });

        let rendered = provenance_image(Some(&value));

        assert!(rendered.starts_with("has unknown status (future_status"));
        assert!(rendered.contains("body="));
        assert!(rendered.contains(r#"\u001b"#));
        assert!(rendered.contains(r#"\u0007"#));
        assert!(!rendered.bytes().any(|byte| byte < 0x20));
    }

    #[test]
    fn restart_budget_hides_matching_lifetime_count() {
        let module = serde_json::json!({
            "restart_count": 2,
            "max_restarts": 3,
            "lifetime_restarts": 2,
        });

        assert_eq!(format_restart_budget(&module), "2 of 3");
    }

    #[test]
    fn restart_budget_shows_lifetime_count_after_budget_reset() {
        let module = serde_json::json!({
            "restart_count": 0,
            "max_restarts": 3,
            "lifetime_restarts": 2,
        });

        assert_eq!(format_restart_budget(&module), "0 of 3 (2 lifetime)");
    }

    #[test]
    fn unknown_without_a_probe_stamp_warms_but_unknown_after_a_probe_alerts() {
        // The supervisor entry carries `"last_probe_ms": null` on the wire for
        // a never-probed module (serde default without skip); the health entry
        // omits the key. Both spellings mean never probed. The fixture uses
        // the null form on the module row because that is what the running
        // daemon sends, and it is the form that read as "probed" once.
        let modules = [
            json!({"module_id": "aft", "state": "running", "last_probe_ms": null}),
            json!({"module_id": "synapse", "state": "running", "last_probe_ms": 1}),
        ];
        let health = [
            json!({"module_id": "aft", "status": "unknown"}),
            json!({"module_id": "synapse", "status": "unknown", "last_probe_ms": 1}),
        ];

        assert!(dashboard_module_is_warming(&health, &modules[0]));
        assert!(!dashboard_module_is_warming(&health, &modules[1]));
        assert_eq!(dashboard_alerts_line(&json!({})), None);
    }

    #[test]
    fn dashboard_alert_line_requires_drops_in_every_window_minute() {
        let scattered = json!({
            "counters": {
                "module_frames_dropped_no_route_last_10m": 9,
                "module_frames_dropped_no_route_nonzero_minutes_last_10m": 9,
                "module_frames_dropped_no_route_by_module": { "alpha": 9 }
            }
        });
        assert_eq!(
            dashboard_alerts_line(&scattered),
            None,
            "scattered drops must not create a dashboard alarm"
        );

        let sustained = json!({
            "counters": {
                "module_frames_dropped_no_route_last_10m": 14,
                "module_frames_dropped_no_route_nonzero_minutes_last_10m": 10,
                "module_frames_dropped_no_route_by_module": { "alpha": 5, "omega": 9 }
            }
        });
        assert_eq!(
            dashboard_alerts_line(&sustained),
            Some("alerts: frame drops (14 in 10m, top: omega)".to_string())
        );
    }

    #[test]
    fn module_status_detail_names_only_its_own_frame_drops() {
        let describe = json!({
            "counters": {
                "module_frames_dropped_no_route_by_module": { "alpha": 3, "beta": 1 }
            }
        });
        assert_eq!(module_frame_drop_count(&describe, "alpha"), Some(3));
        assert_eq!(module_frame_drop_count(&describe, "idle"), None);
    }

    #[test]
    fn age_formatter_uses_operator_scale_at_every_boundary() {
        let now = 2 * 86_400 * 1_000;
        for (delta_seconds, expected) in [
            (0, "just now"),
            (59, "59s ago"),
            (60, "1m ago"),
            (59 * 60, "59m ago"),
            (60 * 60, "1h ago"),
            (23 * 60 * 60, "23h ago"),
            (24 * 60 * 60, "1d ago"),
        ] {
            assert_eq!(
                format_age_from_epoch_ms(now - delta_seconds * 1_000, now),
                expected
            );
        }
        assert_eq!(format_age_from_epoch_ms(now + 5_000, now), "in 5s");
    }

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
            "account": "operator@example.com",
            "accountInfo": { "email": "operator@example.com", "planType": "pro" },
            "savedResets": { "availableCount": 4 }
        });
        let extras = quota_account_header_extras(&enriched);
        // email is the primary label upstream, never repeated in extras.
        assert_eq!(extras.len(), 2, "extras: {extras:?}");
        assert_eq!(extras[0], "plan: pro");
        assert!(extras[1].starts_with("✦ 4 saved resets"));
    }

    /// A `stale` disclosure renders the BLIND duration (now - since), never
    /// the reading's age (now - fetchedAt): the two answer different
    /// questions, and the renderer must not reintroduce the conflation the
    /// wire field exists to prevent.
    #[test]
    fn stale_disclosure_renders_blind_duration_not_reading_age() {
        // No disclosure -> no segment; the fresh path is unchanged.
        let fresh = serde_json::json!({ "provider": "codex" });
        assert!(quota_stale_segment(&fresh).is_none());

        // A reading fetched ~2h ago whose refresh began failing ~5m ago: the
        // segment must carry the minutes, not the hours. If the renderer
        // read fetchedAt instead of stale.since, this asserts red.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let iso = |secs: u64| {
            // Render an RFC3339 UTC timestamp without pulling a date crate
            // into the test: reuse the parser as the round-trip oracle.
            let days = secs / 86_400;
            let (mut y, mut rem) = (1970u64, days);
            loop {
                let len = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                    366
                } else {
                    365
                };
                if rem < len {
                    break;
                }
                rem -= len;
                y += 1;
            }
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            let lens = [
                31,
                if leap { 29 } else { 28 },
                31,
                30,
                31,
                30,
                31,
                31,
                30,
                31,
                30,
                31,
            ];
            let mut m = 0;
            while rem >= lens[m] {
                rem -= lens[m];
                m += 1;
            }
            format!(
                "{y:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                m + 1,
                rem + 1,
                (secs % 86_400) / 3600,
                (secs % 3600) / 60,
                secs % 60
            )
        };
        let fetched_at = iso(now - 7_200);
        let since = iso(now - 300);
        // The oracle: the hand-rolled formatter must agree with the parser
        // the renderer uses, or the assertions below test nothing.
        assert_eq!(parse_rfc3339_to_utc_secs(&since), Some(now - 300));

        let entry = serde_json::json!({
            "provider": "codex",
            "fetchedAt": fetched_at,
            "stale": { "since": since, "class": "upstream_failed" }
        });
        let segment = quota_stale_segment(&entry).expect("disclosure renders");
        assert!(
            segment.contains("5m") && !segment.contains("2h"),
            "must render blind time, not reading age: {segment}"
        );
        assert!(segment.contains("upstream_failed"), "{segment}");

        // A classless disclosure still renders, with the cause named absent.
        let unclassified = serde_json::json!({
            "provider": "codex",
            "stale": { "since": since }
        });
        let segment = quota_stale_segment(&unclassified).expect("renders");
        assert!(segment.contains("cause unstated"), "{segment}");
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
    fn spend_pools_render_as_lines_and_the_three_wire_states_stay_distinct() {
        // The deepseek shape: pools present, usage empty. The account holds
        // money and must not render as "no limits reported".
        let with_pools = json!({
            "usage": {},
            "spend": [
                { "id": "granted_balance", "funding": "granted", "basis": "reported",
                  "remaining": { "minor": 0, "exponent": 2, "unit": "CNY" } },
                { "id": "topped_up_balance", "funding": "purchased", "basis": "reported",
                  "remaining": { "minor": 2402, "exponent": 2, "unit": "CNY" } },
            ],
        });
        let lines = quota_spend_lines_for_entry(&with_pools);
        assert_eq!(
            lines,
            vec!["credit 0.00 CNY (granted)", "credit 24.02 CNY (purchased)"],
            "each pool renders its own line with minor/10^exponent and verbatim unit"
        );

        // The codex shape: the producer asked, the provider has no credit
        // product. `[]` must produce zero lines, never a "0 credit" line.
        let empty_pools = json!({ "usage": {}, "spend": [] });
        assert!(quota_spend_lines_for_entry(&empty_pools).is_empty());

        // Most providers: spend absent entirely. Also zero lines.
        let absent = json!({ "usage": {} });
        assert!(quota_spend_lines_for_entry(&absent).is_empty());

        // A unit that is not a currency code renders verbatim (minimax
        // reports "credit" when the provider states no currency).
        let free_unit = json!({
            "spend": [{ "funding": "granted",
                "remaining": { "minor": 5, "exponent": 0, "unit": "credit" } }],
        });
        assert_eq!(
            quota_spend_lines_for_entry(&free_unit),
            vec!["credit 5 credit (granted)"]
        );
    }

    #[test]
    fn minor_amount_formatting_is_integer_math_with_the_exponent_honoured() {
        assert_eq!(format_minor_amount(2402, 2), "24.02");
        assert_eq!(format_minor_amount(5, 2), "0.05");
        assert_eq!(format_minor_amount(-2402, 2), "-24.02");
        assert_eq!(format_minor_amount(7, 0), "7");
        // A hostile exponent must not panic the renderer.
        assert_eq!(format_minor_amount(1, -3), "1");
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

    /// This fixture pins the bytes emitted by the `--json` renderer. Human-only
    /// dashboard and footer changes must not alter machine-consumed output.
    #[test]
    fn module_rescan_json_output_is_byte_stable_against_fixture() {
        let reply = serde_json::json!({
            "added": [],
            "changed_pending_reload": [],
            "op": "supervisor.rescan",
            "preview": true,
            "removed": [],
            "unchanged": 3
        });
        let expected = "{\n  \"added\": [],\n  \"changed_pending_reload\": [],\n  \"op\": \"supervisor.rescan\",\n  \"preview\": true,\n  \"removed\": [],\n  \"unchanged\": 3\n}";
        assert_eq!(format_json_output(&reply).unwrap(), expected);
    }

    #[test]
    fn quota_json_output_is_byte_stable_against_fixture() {
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

    /// The incident QTA relayed from insula#8: a provider whose EVERY account is
    /// degraded must stay a named entry in the DEFAULT view. Its absence reads
    /// as unconfigured -- the one meaning it is not -- and sends the operator
    /// to re-check provider bindings instead of the credential. Asserted at the
    /// table-selection layer (the layer that dropped it), with the inert
    /// neighbour proving the filter still excludes what it should.
    #[test]
    fn a_fully_degraded_provider_stays_in_the_default_table() {
        let providers = vec![
            serde_json::json!({
                "provider": "claude", "error": "credential requires authentication",
                "errorClass": "credential_unusable"
            }),
            serde_json::json!({ "provider": "idle", "error": "x", "errorClass": "credential_absent" }),
        ];
        let entries = quota_entries_for_table(&providers, None, false);
        let ids: Vec<_> = entries.iter().map(|e| provider_id(e)).collect();
        assert!(
            ids.contains(&"claude".to_string()),
            "a classified degraded provider must render as a named section, got {ids:?}"
        );
        assert!(
            !ids.contains(&"idle".to_string()),
            "an inert never-configured provider stays summary-only, got {ids:?}"
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

    #[test]
    fn bare_ck_is_dashboard_but_bare_json_stays_byte_compatible_help() {
        let bare = parse_args([OsString::from("ck")]).unwrap();
        assert!(matches!(bare.command, Command::Dashboard));

        let json = parse_args([OsString::from("ck"), OsString::from("--json")]).unwrap();
        assert!(matches!(json.command, Command::Help(text) if text == top_help()));
    }

    #[test]
    fn stderr_truncation_hint_reports_shown_total_and_dropped_lines() {
        let response = serde_json::to_value(StderrTail {
            capture: StderrCaptureState::Captured,
            entries: vec![
                StderrTailEntry::Line {
                    text: "first".to_string(),
                    truncated: false,
                },
                StderrTailEntry::ProcessStart,
                StderrTailEntry::Line {
                    text: "last".to_string(),
                    truncated: false,
                },
            ],
            dropped_lines: 3,
        })
        .unwrap();
        assert_eq!(
            stderr_truncation_hint(&response).as_deref(),
            Some("(showing 2 of 5 lines · dropped 3 — use -n <count> for more)")
        );
        assert_eq!(
            stderr_truncation_hint(&serde_json::json!({
                "entries": [{ "kind": "line", "text": "complete" }]
            })),
            None
        );
    }

    #[test]
    fn error_footers_are_scoped_and_absent_for_json() {
        let unknown_module = decorate_error(
            CkError::Rejected("module_id 'm' is not supervised".to_string()),
            false,
            None,
        );
        assert!(unknown_module.to_string().contains("ck module list"));

        let unknown_provider = decorate_error(
            CkError::Rejected("unknown provider 'p'".to_string()),
            true,
            None,
        );
        assert!(!unknown_provider.to_string().contains("help["));
    }

    #[test]
    fn module_terminals_is_parsed_and_documented() {
        let command = parse_command(
            "module",
            &[OsString::from("terminals"), OsString::from("aft")],
        )
        .unwrap();
        assert!(matches!(
            command,
            Command::Module(ModuleCommand::Terminals { module_id }) if module_id == "aft"
        ));
        assert!(MODULE_HELP.contains("ck module terminals <id>"));
    }

    #[test]
    fn module_release_is_parsed_and_documented() {
        let command = parse_command(
            "module",
            &[OsString::from("release"), OsString::from("vault")],
        )
        .unwrap();
        assert!(matches!(
            command,
            Command::Module(ModuleCommand::ReleaseReserved { module_id }) if module_id == "vault"
        ));
        assert!(MODULE_HELP.contains("ck module release <id>"));
    }

    #[test]
    fn routes_command_accepts_an_optional_module_id() {
        let all = parse_command("routes", &[]).unwrap();
        assert!(matches!(all, Command::Routes { module_id: None }));

        let one = parse_command("routes", &[OsString::from("aft")]).unwrap();
        assert!(matches!(
            one,
            Command::Routes {
                module_id: Some(module_id)
            } if module_id == "aft"
        ));
    }

    #[test]
    fn setup_and_upgrade_command_surfaces_are_parsed_and_documented() {
        let test_tail = |tokens: &[&str]| tokens.iter().map(OsString::from).collect::<Vec<_>>();
        let bare = parse_command("setup", &[]).unwrap();
        assert!(matches!(
            bare,
            Command::Setup(setup::SetupRequest {
                optional_components,
                uninstall: false,
                dry_run: false,
                convert: None,
                conversion_confirmed: false,
                ..
            }) if optional_components.is_empty()
        ));

        for component in ["aft", "mc"] {
            let command = parse_command("setup", &test_tail(&[component])).unwrap();
            assert!(matches!(
                command,
                Command::Setup(setup::SetupRequest {
                    optional_components,
                    ..
                }) if optional_components == [parse_setup_component(component).unwrap()]
            ));
        }

        let with = parse_command("setup", &test_tail(&["--with", "aft,mc"])).unwrap();
        assert!(matches!(
            with,
            Command::Setup(setup::SetupRequest {
                optional_components,
                ..
            }) if optional_components == [setup::Component::Aft, setup::Component::Mc]
        ));

        for component in ["aft", "mc"] {
            let command = parse_command("setup", &test_tail(&[component, "--convert"])).unwrap();
            assert!(matches!(
                command,
                Command::Setup(setup::SetupRequest {
                    convert: Some(convert),
                    conversion_confirmed: false,
                    ..
                }) if convert == parse_setup_component(component).unwrap()
            ));
        }

        let uninstall = parse_command("setup", &test_tail(&["--uninstall"])).unwrap();
        assert!(matches!(
            uninstall,
            Command::Setup(setup::SetupRequest {
                uninstall: true,
                ..
            })
        ));
        let dry_run = parse_command("setup", &test_tail(&["--dry-run"])).unwrap();
        assert!(matches!(
            dry_run,
            Command::Setup(setup::SetupRequest { dry_run: true, .. })
        ));
        assert!(matches!(
            parse_command("upgrade", &[]).unwrap(),
            Command::Upgrade {
                check: false,
                verbose: false
            }
        ));
        assert!(matches!(
            parse_command("upgrade", &test_tail(&["--check"])).unwrap(),
            Command::Upgrade {
                check: true,
                verbose: false
            }
        ));
        assert!(matches!(
            parse_command("upgrade", &test_tail(&["--verbose", "--check"])).unwrap(),
            Command::Upgrade {
                check: true,
                verbose: true
            }
        ));
        assert!(SETUP_HELP.contains("without changing anything"));
        assert!(!SETUP_HELP.contains("installation mutator"));
        assert!(!UPGRADE_HELP.contains("MC is wiring-only in alpha"));
        assert!(MODULE_HELP
            .contains("forget a removed module's reserved id so another module may use it"));
        assert!(top_help().contains("setup"));
        assert!(top_help().contains("upgrade"));
    }

    fn triage_fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    fn write_triage_connection(path: &Path, pid: u32, key: &str) {
        fs::write(
            path,
            format!(
                r#"{{"schema":1,"wire_version":1,"endpoints":[{{"host":"127.0.0.1","port":8757}}],"key":"{key}","daemon_id":"daemon-test","pid":{pid}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn daemon_triage_absent_everything_reports_named_findings_and_down() {
        let dir = triage_fixture_dir("absent");
        let connection = dir.join(CONNECTION_FILE_NAME);
        let report = collect_daemon_triage(std::slice::from_ref(&connection), &dir);
        assert_eq!(report.exit_code, 2);
        assert_eq!(report.json["verdict"]["status"], "daemon-appears-down");
        assert_eq!(report.json["verdict"]["reason"], "no connection file");
        assert_eq!(
            report.json["start_lock"]["candidates"][0]["finding"],
            "start-lock absent"
        );
        assert_eq!(
            report.json["connection_file"]["candidates"][0]["finding"],
            "connection file absent"
        );
        assert_eq!(
            report.json["process_liveness"]["skipped"],
            "no pid recovered from connection file or start-lock"
        );
        assert_eq!(report.json["log_tail"]["finding"], "log absent");
    }

    #[tokio::test]
    async fn daemon_triage_command_completes_without_a_daemon() {
        let dir = triage_fixture_dir("command-absent");
        let connection = dir.join(CONNECTION_FILE_NAME);
        let result = run([
            OsString::from("ck"),
            OsString::from("daemon"),
            OsString::from("triage"),
            OsString::from("--subc"),
            connection.clone().into_os_string(),
        ])
        .await;
        assert!(matches!(result, Err(CkError::TriageExit { exit_code: 2 })));
    }

    #[test]
    fn daemon_triage_fresh_connection_with_live_own_pid_is_live() {
        let dir = triage_fixture_dir("live");
        let connection = dir.join(CONNECTION_FILE_NAME);
        write_triage_connection(&connection, process::id(), "never-print-this-key");
        let report = collect_daemon_triage(std::slice::from_ref(&connection), &dir);
        assert_eq!(report.exit_code, 0);
        assert_eq!(report.json["verdict"]["status"], "daemon-appears-live");
        assert_eq!(report.json["process_liveness"]["status"], "live");
    }

    #[test]
    fn daemon_triage_fresh_connection_with_dead_pid_is_ambiguous() {
        let dir = triage_fixture_dir("dead");
        let connection = dir.join(CONNECTION_FILE_NAME);
        // Spawn the test binary itself as the short-lived child: libtest's
        // --help exits immediately, and current_exe needs no shell on PATH
        // (spawning "sh" here passed on Windows CI only by Git-toolchain
        // runner luck -- the fixture class that broke fleet_lint's tests).
        let mut child = process::Command::new(std::env::current_exe().unwrap())
            .arg("--help")
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        write_triage_connection(&connection, pid, "dead-key");
        let report = collect_daemon_triage(std::slice::from_ref(&connection), &dir);
        assert_eq!(report.exit_code, 3);
        assert_eq!(report.json["verdict"]["status"], "daemon-state-ambiguous");
        assert!(report.json["verdict"]["reason"]
            .as_str()
            .unwrap()
            .contains("dead pid"));
    }

    #[test]
    fn daemon_triage_malformed_connection_is_named_and_ambiguous() {
        let dir = triage_fixture_dir("malformed");
        let connection = dir.join(CONNECTION_FILE_NAME);
        fs::write(&connection, br#"{"schema":1,"wire_version":1"#).unwrap();
        let report = collect_daemon_triage(std::slice::from_ref(&connection), &dir);
        assert_eq!(report.exit_code, 3);
        assert_eq!(report.json["verdict"]["status"], "daemon-state-ambiguous");
        assert!(report.json["connection_file"]["candidates"][0]["finding"]
            .as_str()
            .unwrap()
            .contains("JSON parse failure"));
    }

    #[test]
    fn daemon_triage_log_tail_is_capped_and_contains_only_the_tail() {
        let dir = triage_fixture_dir("log");
        let connection = dir.join(CONNECTION_FILE_NAME);
        let log = dir.join("subc.log");
        let mut contents = String::from("oldest-line\n");
        contents.push_str(&"x".repeat((TRIAGE_LOG_MAX_BYTES as usize) + 1024));
        contents.push_str("\nnewest-line\n");
        fs::write(&log, contents).unwrap();
        let report = collect_daemon_triage(std::slice::from_ref(&connection), &dir);
        let log_fact = &report.json["log_tail"];
        assert!(log_fact["summary"]
            .as_str()
            .unwrap()
            .contains("read capped"));
        let rendered = serde_json::to_string(log_fact).unwrap();
        assert!(rendered.contains("newest-line"));
        assert!(!rendered.contains("oldest-line"));
    }

    #[test]
    fn daemon_triage_json_golden_keeps_arm_fields_and_skipped_distinct() {
        let dir = triage_fixture_dir("json");
        let connection = dir.join(CONNECTION_FILE_NAME);
        let report = collect_daemon_triage(std::slice::from_ref(&connection), &dir);
        let object = report.json.as_object().unwrap();
        // Compare SORTED keys: serde_json::Map iteration order is a feature
        // flag, not a behavior. A lone `-p subc-core` build iterates
        // alphabetically, while a workspace build unifies features with
        // agent-token-vectors (whose RFC 8785 canonicalizer legitimately
        // enables `preserve_order`), flipping iteration to insertion order --
        // so an order-sensitive assertion here passes or fails based on which
        // crates were built alongside this one.
        let mut keys = object.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            [
                "connection_file",
                "log_tail",
                "process_liveness",
                "run_dir",
                "start_lock",
                "verdict"
            ]
        );
        assert_eq!(
            report.json["connection_file"]["candidates"][0]["status"],
            "absent"
        );
        assert_eq!(report.json["process_liveness"]["status"], "skipped");
        assert!(report.json["process_liveness"].get("skipped").is_some());
        assert!(report.json["connection_file"]["candidates"][0]
            .get("skipped")
            .is_none());
    }

    #[test]
    fn daemon_triage_never_prints_connection_key_in_any_report_arm() {
        let dir = triage_fixture_dir("key");
        let connection = dir.join(CONNECTION_FILE_NAME);
        let key = "known-secret-key-literal";
        write_triage_connection(&connection, process::id(), key);
        let report = collect_daemon_triage(std::slice::from_ref(&connection), &dir);
        let json_output = serde_json::to_string(&report.json).unwrap();
        assert!(!json_output.contains(key));
        assert!(!report.text.contains(key));
    }

    #[test]
    fn daemon_triage_help_is_help_before_verb_execution() {
        let command = parse_command(
            "daemon",
            &[OsString::from("triage"), OsString::from("--help")],
        )
        .unwrap();
        assert!(matches!(command, Command::Help(text) if text.contains("daemon triage")));
    }
}
