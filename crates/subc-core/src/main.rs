#![forbid(unsafe_code)]

use std::{path::PathBuf, process};

use cortexkit_log::{Config, SegmentRetention};

#[tokio::main]
async fn main() {
    // Side-effect-free provenance probes: evaluated before tracing, bootstrap, or
    // any runtime state so neither touches the start-lock nor reports an
    // already-running daemon.
    //
    // HELP MUST BE HANDLED HERE FOR THE SAME REASON --version IS. Without it, a help
    // request falls through into bootstrap and RUNS THE DAEMON STARTUP PATH: today
    // it stops at the singleton lock and logs "subc daemon already running", which
    // looks harmless and is safe only by CIRCUMSTANCE -- the circumstance being that
    // a daemon happens to be up. On a machine where none is, the same invocation
    // claims the start-lock, publishes a connection file and binds the port. An
    // operator asking a daemon binary what its flags are would start it.
    //
    // Scanned across all arguments rather than only the first, because the shape
    // someone types is a real invocation with the flag appended.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("ck-subc {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // PARSE-THEN-ACT: EVERY argument is settled before the first side effect, not
    // just the two recognised ones. Handling --help and --version early while
    // letting anything else fall through leaves the original defect for every OTHER
    // argument -- a typo, a flag copied from another tool, a stale invocation -- all
    // of which would silently START A DAEMON. The daemon takes no arguments, so the
    // complete rule is: recognise the two probes, refuse everything else, and only
    // then bootstrap.
    if let Some(unknown) = args
        .iter()
        .find(|arg| *arg != "--help" && *arg != "-h" && *arg != "help" && *arg != "--version")
    {
        eprintln!(
            "ck-subc: unexpected argument '{}'\n\nck-subc takes no arguments; \
             it is started by launchd and reads subc.jsonc from the XDG config \
             directory. Use `ck` to inspect or control a running daemon, or \
             `ck-subc --help`.",
            unknown.to_string_lossy()
        );
        process::exit(2);
    }
    if args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help")
    {
        println!(
            "ck-subc {} — the CortexKit subc daemon\n\n\
             Started by launchd; it takes no arguments and reads its configuration\n\
             from subc.jsonc under the XDG config directory.\n\n\
             flags:\n  \
               --version   print the version and exit\n  \
               --help      print this and exit\n\n\
             To inspect or control a running daemon use `ck` (`ck module list`,\n\
             `ck health`, `ck daemon`). Running this binary directly starts a daemon.",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }

    // Refuse an invalid placement override before logging or bootstrap can write state.
    let placement = match cgroup_placement(std::env::var_os("SUBC_CGROUP_PLACEMENT").as_deref()) {
        Ok(placement) => placement,
        Err(error) => {
            eprintln!("ck-subc: refusing to start: {error}");
            process::exit(1);
        }
    };

    // Resolve the run directory before anything writes into it. A relative data
    // home would otherwise put the daemon's logs, journal and connection state
    // under whatever directory it was started from, so the daemon refuses to
    // start and names the variables to set. This goes to stderr because the
    // log sink lives inside the directory that could not be resolved.
    let run_dir = match subc_daemon::daemon_config::daemon_run_dir() {
        Ok(run_dir) => run_dir,
        Err(error) => {
            eprintln!("ck-subc: refusing to start: {error}");
            process::exit(1);
        }
    };

    if let Err(err) = init_tracing(&run_dir) {
        eprintln!("ck-subc: failed to initialize logging: {err}");
        process::exit(1);
    }

    let daemon = async {
        let config = subc_daemon::bootstrap::BootstrapConfig::from_env_for_daemon_binary()?
            .with_cgroup_placement(placement);
        subc_daemon::bootstrap::run_with_config(config).await
    };
    if let Err(err) = daemon.await {
        tracing::error!(error = %err, "subc-core failed");
        eprintln!("subc-core: {err}");
        process::exit(1);
    }
    // The bounded SIGTERM path has already announced the shutdown, closed every
    // connection so modules saw EOF, and ended any child still running at its
    // own bounded deadline. Exit without dropping the Tokio runtime: tearing down every
    // task adds nothing at this point, and supervised Child handles use
    // kill_on_drop, so a destructor would be a second, unbounded-order kill
    // path next to the deliberate one. Process exit closes the descriptors.
    process::exit(0);
}

/// The binary uses the caller's cgroup in production; test harnesses can opt out
/// without changing the default for installed daemons.
fn cgroup_placement(
    value: Option<&std::ffi::OsStr>,
) -> Result<subc_daemon::bootstrap::CgroupPlacementConfig, String> {
    use subc_daemon::bootstrap::CgroupPlacementConfig;
    match value {
        None => Ok(CgroupPlacementConfig::Current),
        Some(value) if value == "disabled" => Ok(CgroupPlacementConfig::Disabled),
        Some(value) => Err(format!(
            "SUBC_CGROUP_PLACEMENT must be 'disabled' when set (got {:?})",
            value
        )),
    }
}

fn init_tracing(run_dir: &std::path::Path) -> Result<(), cortexkit_log::InitError> {
    let config_path = subc_daemon::daemon_config::default_config_path();
    // THIS eprintln! MUST STAY ON STDERR. It is the daemon's only pre-subscriber
    // channel: a failure to read logging config happens BEFORE the file sink exists,
    // so a message about it cannot go to the file sink. Everything else the daemon
    // says goes to subc.log (see install_tracing below), which makes this arm look
    // like dead code precisely when the daemon is working -- it only ever fires when
    // logging itself is broken. A consumer's hermetic test lane keeps the daemon's
    // piped stderr as post-mortem text in failure messages for exactly this line;
    // folding it into the file sink would silence the one report that cannot use it.
    let logging = subc_daemon::daemon_config::load_logging(&config_path)
        .map_err(|error| {
            eprintln!(
                "ck-subc: could not read daemon logging config from {}: {error}; using defaults",
                config_path.display()
            );
        })
        .ok()
        .flatten();
    // Secure the run directory BEFORE anything opens a sink inside it. The log
    // sink creates its parents with create_dir_all, which lands 0755 on a default
    // desk, and whichever creator runs first fixes the mode for every later one --
    // the connection-file writer builds parents at 0700 but returns early when the
    // directory exists. Doing it here makes the daemon the first creator on a
    // clean box and the tightener on an existing one.
    match subc_daemon::daemon_config::ensure_daemon_run_dir_private() {
        Ok(_) => {}
        Err(error) => eprintln!(
            "ck-subc: could not secure the run directory at {}: {error}; continuing, \
             but another account on this host may be able to list it",
            run_dir.display()
        ),
    }
    let logs_dir = run_dir.join("logs");
    install_tracing(daemon_logger_config(logs_dir, logging.as_ref()))
}

// The daemon logs as module `subc` into `run/logs/subc.<YYYY-MM-DD>.log`, the
// same r2 segment shape every module writes, so `ck module logs` and any
// tail-by-date reader treat it like the rest of the fleet. `run/logs/` is not
// a module data directory, which is why the path is assembled here rather than
// through `Config::for_module`.
fn daemon_logger_config(
    logs_dir: PathBuf,
    logging: Option<&subc_daemon::daemon_config::LoggingConfig>,
) -> Config {
    Config {
        module_id: "subc".to_string(),
        logs_dir,
        bound: Vec::new(),
        spec: logging.map(|config| config.filter_spec("subc")),
        retention: logging.map_or_else(
            SegmentRetention::default,
            subc_daemon::daemon_config::LoggingConfig::segment_retention,
        ),
        redactor: None,
        clock: None,
    }
}

fn install_tracing(config: Config) -> Result<(), cortexkit_log::InitError> {
    // cortexkit-log owns one process-global file sink and does not expose a
    // cheap tee layer. The daemon therefore writes directly to its dated
    // segment only; stdout is intentionally not a second logging destination.
    cortexkit_log::init(config).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::Arc,
        time::{Duration, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn cgroup_placement_override_requires_exact_disabled_value() {
        use subc_daemon::bootstrap::CgroupPlacementConfig;
        assert_eq!(cgroup_placement(None), Ok(CgroupPlacementConfig::Current));
        assert_eq!(
            cgroup_placement(Some(std::ffi::OsStr::new("disabled"))),
            Ok(CgroupPlacementConfig::Disabled)
        );
        let error = cgroup_placement(Some(std::ffi::OsStr::new("current"))).unwrap_err();
        assert!(error.contains("SUBC_CGROUP_PLACEMENT"), "{error}");
        assert!(cgroup_placement(Some(std::ffi::OsStr::new(""))).is_err());
    }

    #[test]
    fn daemon_log_line_matches_the_authority_fixture_byte_for_byte_without_ansi() {
        let temp = subc_test_support::TestTempDir::new("subc-daemon-log-format");
        let logs_dir = temp.path().to_path_buf();
        let mut config = daemon_logger_config(logs_dir.clone(), None);
        config.module_id = "fusiform".to_string();
        config.clock = Some(Arc::new(|| {
            UNIX_EPOCH + Duration::from_millis(1_788_604_863_123)
        }));
        install_tracing(config).unwrap();

        tracing::info!(
            version = 1_788_526_509_641_u64,
            eras = 22_u64,
            facts_changed = 0_u64,
            arrived = 2_u64,
            "poll changed"
        );
        // The clock is pinned to 2026-09-05, so the segment is that day's.
        let line = fs::read_to_string(logs_dir.join("fusiform.2026-09-05.log")).unwrap();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/log_format_golden.json")).unwrap();
        let expected = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == "plain-info-no-bound")
            .unwrap()["line"]
            .as_str()
            .unwrap();
        assert_eq!(line, format!("{expected}\n"));
        assert!(!line.contains('\u{1b}'));

        // THE DAEMON'S OWN LOGGER NAMES, READ BACK FROM THE FILE. A `target:`
        // string is opaque to the compiler and checked by nothing until an
        // operator filters on it, and by then the symptom is silence: under
        // r2 a `::` path target maps to the BARE module id, so the four
        // `target: "subc_daemon::control"` sites in control.rs rendered as
        // root `subc:` for a week while looking like they named a component.
        // The subscriber is process-global, which is why this lives in the
        // same test as the install above rather than beside it.
        tracing::info!(target: "control", "component line");
        tracing::info!(target: "subc_daemon::control", "path-shaped target");
        let lines = fs::read_to_string(logs_dir.join("fusiform.2026-09-05.log")).unwrap();
        let rendered: Vec<&str> = lines.lines().collect();
        assert_eq!(rendered.len(), 3, "{lines}");
        assert!(
            rendered[1].contains(" fusiform.control: component line"),
            "a segment-grammar target must render as <module>.<component>: {}",
            rendered[1]
        );
        assert!(
            rendered[2].contains(" fusiform: path-shaped target"),
            "a `::` target must map to the bare module id, never a component: {}",
            rendered[2]
        );
    }
}
