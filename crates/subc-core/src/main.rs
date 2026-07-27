#![forbid(unsafe_code)]

use std::process;

use tracing_subscriber::EnvFilter;

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

    init_tracing();

    if let Err(err) = subc_core::bootstrap::run().await {
        tracing::error!(error = %err, "subc-core failed");
        eprintln!("subc-core: {err}");
        process::exit(1);
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
